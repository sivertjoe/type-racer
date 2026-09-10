use std::collections::{HashMap, HashSet};

use crate::protocol::{ClientMessage, ClientProgress, GameConfig, GameProgress, Pacing, RacerProgress, RoundOverInfo, ServerMessage};
use crate::session::{HubEffect, Session};
use crate::waiting::ClientHandle;

/// "Knockout": a sequence of ordinary typing-race rounds among a
/// shrinking pool of "active" competitors. Eliminated players stay
/// connected as spectators (they still receive round content and live
/// progress, just as `Spectating` instead of `GameStarting`, and can't
/// send `Progress`). Once exactly two remain, rounds become a best-of-3 -
/// nobody is eliminated further, this just tracks games won until someone
/// reaches 2. Its own little server, constructed fresh each tournament
/// and handed the connections' messages directly by
/// [`crate::session::Server`] - it doesn't know anything about the lobby
/// or about other gamemodes.
pub struct KnockoutServer {
    pacing: Pacing,
    /// Every participant that was in the tournament at the start, so
    /// placements can be reported by name even after someone's client
    /// disconnects.
    usernames: HashMap<u32, String>,
    active: HashSet<u32>,
    /// Elimination groups in order, worst placement first. Each group is
    /// already sorted best-to-worst within itself (by progress, for
    /// simultaneous eliminations from a host-cut round).
    eliminated_order: Vec<Vec<u32>>,
    is_finals: bool,
    finals_wins: HashMap<u32, u32>,
    round: RoundState,
    tournament_over: bool,
}

enum RoundState {
    /// Between rounds (including before the first one begins).
    Idle,
    Starting { ready: HashSet<u32> },
    Racing { racers: HashMap<u32, RacerProgress>, next_place: u32 },
}

impl KnockoutServer {
    pub fn new(pacing: Pacing, usernames: HashMap<u32, String>) -> Self {
        let active: HashSet<u32> = usernames.keys().copied().collect();
        let is_finals = active.len() == 2;
        let finals_wins = if is_finals { active.iter().map(|&id| (id, 0)).collect() } else { HashMap::new() };
        Self {
            pacing,
            usernames,
            active,
            eliminated_order: Vec::new(),
            is_finals,
            finals_wins,
            round: RoundState::Idle,
            tournament_over: false,
        }
    }

    fn is_between_rounds(&self) -> bool {
        matches!(self.round, RoundState::Idle)
    }

    /// Starts the next round: a fresh random sentence, sent as
    /// `GameStarting` to active competitors and `Spectating` to everyone
    /// else. Must be called once right after construction, and again
    /// each time [`Session::continue_round`] fires.
    pub fn begin_round(&mut self) -> HubEffect {
        let sentence = crate::words::generate_sentence();
        self.round = RoundState::Starting { ready: HashSet::new() };

        let mut per_client = HashMap::with_capacity(self.usernames.len());
        for &id in self.usernames.keys() {
            let msg = if self.active.contains(&id) {
                ServerMessage::GameStarting { config: GameConfig::TypingRace { sentence: sentence.clone() } }
            } else {
                ServerMessage::Spectating { sentence: sentence.clone() }
            };
            per_client.insert(id, msg);
        }
        HubEffect::Targeted(per_client)
    }

    fn client_ready(&mut self, id: u32) -> HubEffect {
        let RoundState::Starting { ready } = &mut self.round else {
            return HubEffect::None;
        };
        if !self.active.contains(&id) {
            return HubEffect::None; // spectators don't gate the round
        }
        ready.insert(id);
        if self.active.is_subset(ready) { self.begin_race() } else { HubEffect::None }
    }

    fn begin_race(&mut self) -> HubEffect {
        self.round = RoundState::Racing { racers: HashMap::new(), next_place: 1 };
        HubEffect::Broadcast(ServerMessage::GameBegin)
    }

    /// Records `id`'s progress. Once all but one active racer has
    /// finished, the round ends automatically.
    fn client_progress(&mut self, id: u32, username: String, progress: ClientProgress) -> HubEffect {
        let RoundState::Racing { racers, next_place } = &mut self.round else {
            return HubEffect::None;
        };
        if !self.active.contains(&id) {
            return HubEffect::None; // spectators can't report progress
        }

        let place = racers.get(&id).and_then(|existing| existing.place).or_else(|| {
            progress.finished.then(|| {
                let place = *next_place;
                *next_place += 1;
                place
            })
        });
        racers.insert(id, RacerProgress { username, finished: progress.finished, place, detail: progress.detail });

        let finished_count = racers.values().filter(|r| r.finished).count();
        if finished_count + 1 < self.active.len() {
            let racers: Vec<RacerProgress> = racers.values().cloned().collect();
            return HubEffect::Broadcast(ServerMessage::RaceState { racers, all_finished: false });
        }

        let racers = racers.clone();
        self.end_round(racers)
    }

    fn end_round(&mut self, racers: HashMap<u32, RacerProgress>) -> HubEffect {
        let mut not_finished: Vec<u32> = self.active.iter().copied().filter(|id| !racers.get(id).is_some_and(|r| r.finished)).collect();
        // Best progress first, so a tie within one eliminated group still
        // has a sensible internal ranking.
        not_finished.sort_by_key(|id| std::cmp::Reverse(correct_chars(&racers, *id)));
        self.round = RoundState::Idle;

        if self.is_finals {
            return self.finals_round_over(not_finished);
        }

        for id in &not_finished {
            self.active.remove(id);
        }
        if !not_finished.is_empty() {
            self.eliminated_order.push(not_finished.clone());
        }

        if self.active.len() == 1 {
            let winner = *self.active.iter().next().unwrap();
            return self.finish_tournament(winner);
        }

        let entering_finals = self.active.len() == 2;
        if entering_finals {
            self.is_finals = true;
            self.finals_wins = self.active.iter().map(|&id| (id, 0)).collect();
        }

        let info = RoundOverInfo {
            eliminated: not_finished.iter().map(|id| self.usernames[id].clone()).collect(),
            remaining: self.active.iter().map(|id| self.usernames[id].clone()).collect(),
            pacing: self.pacing,
            entering_finals,
            finals_score: None,
        };
        HubEffect::Broadcast(ServerMessage::RoundOver(info))
    }

    fn finals_round_over(&mut self, not_finished: Vec<u32>) -> HubEffect {
        // With exactly two active racers, the round ends the instant one
        // of them finishes, so exactly one is ever "not finished" here.
        let Some(&winner) = self.active.iter().find(|id| !not_finished.contains(id)) else {
            return HubEffect::None;
        };
        *self.finals_wins.entry(winner).or_insert(0) += 1;

        if self.finals_wins[&winner] >= 2 {
            return self.finish_tournament(winner);
        }

        let finals_score = self.active.iter().map(|&id| (self.usernames[&id].clone(), *self.finals_wins.get(&id).unwrap_or(&0))).collect();
        let info = RoundOverInfo {
            eliminated: Vec::new(),
            remaining: self.active.iter().map(|id| self.usernames[id].clone()).collect(),
            pacing: self.pacing,
            entering_finals: false,
            finals_score: Some(finals_score),
        };
        HubEffect::Broadcast(ServerMessage::RoundOver(info))
    }

    fn finish_tournament(&mut self, winner: u32) -> HubEffect {
        let mut standings = vec![winner];
        if let Some(&runner_up) = self.active.iter().find(|&&id| id != winner) {
            standings.push(runner_up);
        }
        for group in self.eliminated_order.iter().rev() {
            standings.extend(group.iter().copied());
        }

        let standings = standings.into_iter().map(|id| self.usernames[&id].clone()).collect();
        self.tournament_over = true;
        HubEffect::Broadcast(ServerMessage::TournamentOver { standings })
    }
}

impl Session for KnockoutServer {
    fn handle_message(&mut self, id: u32, msg: ClientMessage, _clients: &mut HashMap<u32, ClientHandle>) -> HubEffect {
        match msg {
            ClientMessage::ReadyForGame => self.client_ready(id),
            ClientMessage::Progress(progress) => {
                let username = self.usernames.get(&id).cloned().unwrap_or_default();
                self.client_progress(id, username, progress)
            }
            _ => HubEffect::None,
        }
    }

    fn force_begin(&mut self) -> HubEffect {
        match self.round {
            RoundState::Starting { .. } => self.begin_race(),
            RoundState::Idle | RoundState::Racing { .. } => HubEffect::None,
        }
    }

    /// Host-triggered early end to the round: everyone still not
    /// finished is treated as eliminated (or, in the finals, as having
    /// lost this game).
    fn force_end(&mut self) -> HubEffect {
        let RoundState::Racing { racers, .. } = &self.round else {
            return HubEffect::None;
        };
        let racers = racers.clone();
        self.end_round(racers)
    }

    fn continue_round(&mut self) -> HubEffect {
        if self.is_between_rounds() { self.begin_round() } else { HubEffect::None }
    }

    fn concluded(&self) -> bool {
        self.tournament_over
    }
}

fn correct_chars(racers: &HashMap<u32, RacerProgress>, id: u32) -> usize {
    match racers.get(&id).map(|r| &r.detail) {
        Some(GameProgress::TypingRace { correct_chars, .. }) => *correct_chars,
        None => 0,
    }
}

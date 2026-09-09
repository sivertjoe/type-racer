use std::collections::{HashMap, HashSet};

use crate::game_hub::HubEffect;
use crate::protocol::{ClientProgress, GameConfig, GameProgress, RacerProgress, RoundOverInfo, ServerMessage};

/// Orchestrates a knockout tournament: a sequence of ordinary typing-race
/// rounds among a shrinking pool of "active" competitors. Eliminated
/// players stay connected as spectators (they still receive round
/// content and live progress, just as `Spectating` instead of
/// `GameStarting`, and can't send `Progress`). Once exactly two remain,
/// rounds become a best-of-3 - nobody is eliminated further, the hub just
/// tracks games won until someone reaches 2.
pub struct KnockoutHub {
    pacing: crate::protocol::Pacing,
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
}

enum RoundState {
    /// Between rounds (including before the first one begins).
    Idle,
    Starting { ready: HashSet<u32> },
    Racing { racers: HashMap<u32, RacerProgress>, next_place: u32 },
}

impl KnockoutHub {
    pub fn new(pacing: crate::protocol::Pacing, usernames: HashMap<u32, String>) -> Self {
        let active: HashSet<u32> = usernames.keys().copied().collect();
        let is_finals = active.len() == 2;
        let finals_wins = if is_finals { active.iter().map(|&id| (id, 0)).collect() } else { HashMap::new() };
        Self { pacing, usernames, active, eliminated_order: Vec::new(), is_finals, finals_wins, round: RoundState::Idle }
    }

    pub fn is_between_rounds(&self) -> bool {
        matches!(self.round, RoundState::Idle)
    }

    /// Starts the next round: a fresh random sentence, sent as
    /// `GameStarting` to active competitors and `Spectating` to everyone
    /// else.
    pub fn begin_round(&mut self) -> HubEffect {
        let sentence = crate::typing_race::generate_sentence();
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

    pub fn client_ready(&mut self, id: u32) -> HubEffect {
        let RoundState::Starting { ready } = &mut self.round else {
            return HubEffect::None;
        };
        if !self.active.contains(&id) {
            return HubEffect::None; // spectators don't gate the round
        }
        ready.insert(id);
        if self.active.is_subset(ready) { self.begin_race() } else { HubEffect::None }
    }

    pub fn force_begin_round(&mut self) -> HubEffect {
        match self.round {
            RoundState::Starting { .. } => self.begin_race(),
            RoundState::Idle | RoundState::Racing { .. } => HubEffect::None,
        }
    }

    fn begin_race(&mut self) -> HubEffect {
        self.round = RoundState::Racing { racers: HashMap::new(), next_place: 1 };
        HubEffect::Broadcast(ServerMessage::GameBegin)
    }

    /// Records `id`'s progress. Once all but one active racer has
    /// finished, the round ends automatically. Returns the effect to
    /// apply and whether the whole tournament just concluded.
    pub fn client_progress(&mut self, id: u32, username: String, progress: ClientProgress) -> (HubEffect, bool) {
        let RoundState::Racing { racers, next_place } = &mut self.round else {
            return (HubEffect::None, false);
        };
        if !self.active.contains(&id) {
            return (HubEffect::None, false); // spectators can't report progress
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
            return (HubEffect::Broadcast(ServerMessage::RaceState { racers, all_finished: false }), false);
        }

        let racers = racers.clone();
        self.end_round(racers)
    }

    /// Host-triggered early end to the round: everyone still not
    /// finished is treated as eliminated (or, in the finals, as having
    /// lost this game).
    pub fn force_end_round(&mut self) -> (HubEffect, bool) {
        let RoundState::Racing { racers, .. } = &self.round else {
            return (HubEffect::None, false);
        };
        let racers = racers.clone();
        self.end_round(racers)
    }

    fn end_round(&mut self, racers: HashMap<u32, RacerProgress>) -> (HubEffect, bool) {
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
            return (self.finish_tournament(winner), true);
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
        (HubEffect::Broadcast(ServerMessage::RoundOver(info)), false)
    }

    fn finals_round_over(&mut self, not_finished: Vec<u32>) -> (HubEffect, bool) {
        // With exactly two active racers, the round ends the instant one
        // of them finishes, so exactly one is ever "not finished" here.
        let Some(&winner) = self.active.iter().find(|id| !not_finished.contains(id)) else {
            return (HubEffect::None, false);
        };
        *self.finals_wins.entry(winner).or_insert(0) += 1;

        if self.finals_wins[&winner] >= 2 {
            return (self.finish_tournament(winner), true);
        }

        let finals_score = self.active.iter().map(|&id| (self.usernames[&id].clone(), *self.finals_wins.get(&id).unwrap_or(&0))).collect();
        let info = RoundOverInfo {
            eliminated: Vec::new(),
            remaining: self.active.iter().map(|id| self.usernames[id].clone()).collect(),
            pacing: self.pacing,
            entering_finals: false,
            finals_score: Some(finals_score),
        };
        (HubEffect::Broadcast(ServerMessage::RoundOver(info)), false)
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
        HubEffect::Broadcast(ServerMessage::TournamentOver { standings })
    }
}

fn correct_chars(racers: &HashMap<u32, RacerProgress>, id: u32) -> usize {
    match racers.get(&id).map(|r| &r.detail) {
        Some(GameProgress::TypingRace { correct_chars, .. }) => *correct_chars,
        None => 0,
    }
}

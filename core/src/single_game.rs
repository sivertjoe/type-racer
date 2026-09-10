use std::collections::{HashMap, HashSet};

use crate::protocol::{ClientMessage, ClientProgress, GameConfig, RacerProgress, ServerMessage};
use crate::session::{HubEffect, Session};
use crate::waiting::ClientHandle;

/// "Single Game": one typing race among every accepted client, no
/// elimination. This is its own little server, constructed fresh each
/// time one is started and handed the connections' messages directly by
/// [`crate::session::Server`] - it doesn't know anything about the lobby
/// or about other gamemodes.
pub struct SingleGameServer {
    state: State,
}

enum State {
    Starting { ready: HashSet<u32> },
    Racing { racers: HashMap<u32, RacerProgress>, next_place: u32 },
    Concluded,
}

impl SingleGameServer {
    pub fn new() -> Self {
        Self { state: State::Starting { ready: HashSet::new() } }
    }

    /// Broadcasts `GameStarting` with `sentence`. Must be called exactly
    /// once, right after construction.
    pub fn start(&mut self, sentence: String) -> HubEffect {
        HubEffect::Broadcast(ServerMessage::GameStarting { config: GameConfig::TypingRace { sentence } })
    }

    fn client_ready(&mut self, id: u32, participants: &HashSet<u32>) -> HubEffect {
        let State::Starting { ready } = &mut self.state else {
            return HubEffect::None;
        };
        ready.insert(id);
        if participants.is_subset(ready) { self.begin_race() } else { HubEffect::None }
    }

    fn begin_race(&mut self) -> HubEffect {
        self.state = State::Racing { racers: HashMap::new(), next_place: 1 };
        HubEffect::Broadcast(ServerMessage::GameBegin)
    }

    /// Records `id`'s latest progress and returns the updated roster to
    /// broadcast. Assigns a finish place the first time a client reports
    /// `finished`. `all_finished` reflects whether every id in
    /// `participants` has now finished; once true, this game concludes.
    fn client_progress(&mut self, id: u32, username: String, progress: ClientProgress, participants: &HashSet<u32>) -> HubEffect {
        let State::Racing { racers, next_place } = &mut self.state else {
            return HubEffect::None;
        };

        let place = racers.get(&id).and_then(|existing| existing.place).or_else(|| {
            progress.finished.then(|| {
                let place = *next_place;
                *next_place += 1;
                place
            })
        });

        racers.insert(id, RacerProgress { username, finished: progress.finished, place, detail: progress.detail });

        let all_finished = participants.iter().all(|id| racers.get(id).is_some_and(|r| r.finished));
        let racers: Vec<RacerProgress> = racers.values().cloned().collect();

        if all_finished {
            self.state = State::Concluded;
        }

        HubEffect::Broadcast(ServerMessage::RaceState { racers, all_finished })
    }
}

impl Session for SingleGameServer {
    fn handle_message(&mut self, id: u32, msg: ClientMessage, clients: &mut HashMap<u32, ClientHandle>) -> HubEffect {
        let participants: HashSet<u32> = clients.values().filter(|c| c.accepted).map(|c| c.id).collect();
        match msg {
            ClientMessage::ReadyForGame => self.client_ready(id, &participants),
            ClientMessage::Progress(progress) => {
                let username = clients.get(&id).map(|c| c.username.clone()).unwrap_or_default();
                self.client_progress(id, username, progress, &participants)
            }
            _ => HubEffect::None,
        }
    }

    /// Forces the race to begin even if some clients never confirmed
    /// ready (e.g. they've stalled or dropped). Idempotent once begun.
    fn force_begin(&mut self) -> HubEffect {
        match self.state {
            State::Starting { .. } => self.begin_race(),
            State::Racing { .. } | State::Concluded => HubEffect::None,
        }
    }

    /// Ends the race early (host-triggered), reporting whatever progress
    /// racers had made as final. A no-op unless a race is in progress.
    fn force_end(&mut self) -> HubEffect {
        let State::Racing { racers, .. } = &self.state else {
            return HubEffect::None;
        };
        let racers: Vec<RacerProgress> = racers.values().cloned().collect();
        self.state = State::Concluded;
        HubEffect::Broadcast(ServerMessage::RaceState { racers, all_finished: true })
    }

    fn concluded(&self) -> bool {
        matches!(self.state, State::Concluded)
    }
}

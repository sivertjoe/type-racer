use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::protocol::{ClientProgress, GameConfig, RacerProgress, ServerMessage};

/// Server-side authority for "the current game", if any. Doesn't know how
/// to *play* any particular game - it only tracks who's ready and relays
/// progress, keyed by client id. Adding a new game type never touches this
/// file; it only needs a new `GameConfig`/`GameProgress` variant plus
/// client-side handling.
pub struct GameHub {
    state: State,
}

enum State {
    Idle,
    Starting { ready: HashSet<u32> },
    Racing { racers: HashMap<u32, RacerProgress>, next_place: u32 },
}

pub type SharedGameHub = Arc<Mutex<GameHub>>;

/// A state change the caller needs to broadcast to every connected client.
pub enum HubEffect {
    None,
    Broadcast(ServerMessage),
}

impl GameHub {
    pub fn new() -> SharedGameHub {
        Arc::new(Mutex::new(Self { state: State::Idle }))
    }

    /// Begins the ready handshake for a new game.
    pub fn start(&mut self, config: GameConfig) -> HubEffect {
        self.state = State::Starting { ready: HashSet::new() };
        HubEffect::Broadcast(ServerMessage::GameStarting { config })
    }

    /// Records that `id` is ready. Once every id in `connected` has called
    /// this, the race begins.
    pub fn client_ready(&mut self, id: u32, connected: &HashSet<u32>) -> HubEffect {
        let State::Starting { ready, .. } = &mut self.state else {
            return HubEffect::None;
        };
        ready.insert(id);
        if connected.is_subset(ready) { self.begin_race() } else { HubEffect::None }
    }

    /// Forces the race to begin even if some clients never confirmed ready
    /// (e.g. they've stalled or dropped). Idempotent once already begun.
    pub fn force_begin(&mut self) -> HubEffect {
        match self.state {
            State::Starting { .. } => self.begin_race(),
            State::Idle | State::Racing { .. } => HubEffect::None,
        }
    }

    fn begin_race(&mut self) -> HubEffect {
        self.state = State::Racing { racers: HashMap::new(), next_place: 1 };
        HubEffect::Broadcast(ServerMessage::GameBegin)
    }

    /// Records `id`'s latest progress and returns the updated roster to
    /// broadcast. Assigns a finish place the first time a client reports
    /// `finished`. `all_finished` in the broadcast reflects whether every
    /// id in `connected` has now finished.
    pub fn client_progress(&mut self, id: u32, username: String, progress: ClientProgress, connected: &HashSet<u32>) -> HubEffect {
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

        let all_finished = connected.iter().all(|id| racers.get(id).is_some_and(|r| r.finished));

        HubEffect::Broadcast(ServerMessage::RaceState { racers: racers.values().cloned().collect(), all_finished })
    }
}

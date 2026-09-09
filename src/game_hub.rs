use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::knockout::KnockoutHub;
use crate::protocol::{ClientProgress, GameConfig, RacerProgress, ServerMessage};

/// A state change the caller needs to act on: broadcast the same message
/// to everyone, or send different clients different messages (a knockout
/// round starting sends racers `GameStarting` but spectators `Spectating`).
pub enum HubEffect {
    None,
    Broadcast(ServerMessage),
    Targeted(HashMap<u32, ServerMessage>),
}

/// Whichever single game engine (Single Game or Knockout) is currently
/// running, if any. This is the one piece of shared, lockable game state -
/// `waiting.rs` dispatches every gameplay message through it rather than
/// picking between two separate locks.
pub enum ActiveGame {
    Idle,
    SingleGame(GameHub),
    Knockout(KnockoutHub),
}

pub type SharedActiveGame = Arc<Mutex<ActiveGame>>;

impl ActiveGame {
    pub fn new() -> SharedActiveGame {
        Arc::new(Mutex::new(ActiveGame::Idle))
    }

    /// Starts a new game. A no-op unless nothing else is currently
    /// running - guards against e.g. a double Enter-press in the lobby
    /// restarting an already-starting game out from under everyone.
    /// `participants` is every accepted client at the moment of starting.
    pub fn start(&mut self, config: GameConfig, participants: HashMap<u32, String>) -> HubEffect {
        if !matches!(self, ActiveGame::Idle) {
            return HubEffect::None;
        }
        match config {
            GameConfig::TypingRace { sentence } => {
                let mut hub = GameHub::new();
                let effect = hub.start(GameConfig::TypingRace { sentence });
                *self = ActiveGame::SingleGame(hub);
                effect
            }
            GameConfig::Knockout { pacing } => {
                if participants.len() < 2 {
                    return HubEffect::None;
                }
                let mut hub = KnockoutHub::new(pacing, participants);
                let effect = hub.begin_round();
                *self = ActiveGame::Knockout(hub);
                effect
            }
        }
    }

    pub fn client_ready(&mut self, id: u32, participants: &HashSet<u32>) -> HubEffect {
        match self {
            ActiveGame::SingleGame(hub) => hub.client_ready(id, participants),
            ActiveGame::Knockout(hub) => hub.client_ready(id),
            ActiveGame::Idle => HubEffect::None,
        }
    }

    /// Ready-timeout fallback: forces the current round to begin even if
    /// not everyone confirmed ready. A no-op once already begun.
    pub fn force_begin(&mut self) -> HubEffect {
        match self {
            ActiveGame::SingleGame(hub) => hub.force_begin(),
            ActiveGame::Knockout(hub) => hub.force_begin_round(),
            ActiveGame::Idle => HubEffect::None,
        }
    }

    pub fn client_progress(&mut self, id: u32, username: String, progress: ClientProgress, participants: &HashSet<u32>) -> HubEffect {
        let (effect, over) = match self {
            ActiveGame::SingleGame(hub) => {
                let effect = hub.client_progress(id, username, progress, participants);
                (effect, hub.is_idle())
            }
            ActiveGame::Knockout(hub) => hub.client_progress(id, username, progress),
            ActiveGame::Idle => (HubEffect::None, false),
        };
        if over {
            *self = ActiveGame::Idle;
        }
        effect
    }

    /// Host-triggered early end to the current round (Single Game) or
    /// knockout round.
    pub fn force_end(&mut self) -> HubEffect {
        let (effect, over) = match self {
            ActiveGame::SingleGame(hub) => {
                let effect = hub.force_end();
                (effect, hub.is_idle())
            }
            ActiveGame::Knockout(hub) => hub.force_end_round(),
            ActiveGame::Idle => (HubEffect::None, false),
        };
        if over {
            *self = ActiveGame::Idle;
        }
        effect
    }

    /// Starts the next knockout round once the current one is between
    /// rounds (either the host asked to continue, or the auto-pacing
    /// timer fired). A no-op for any other state.
    pub fn continue_knockout_round(&mut self) -> HubEffect {
        match self {
            ActiveGame::Knockout(hub) if hub.is_between_rounds() => hub.begin_round(),
            _ => HubEffect::None,
        }
    }
}

/// The single-round engine shared by Single Game and each round of a
/// Knockout tournament. Doesn't know how to *play* any particular game -
/// it only tracks who's ready and relays progress, keyed by client id.
pub struct GameHub {
    state: State,
}

enum State {
    Idle,
    Starting { ready: HashSet<u32> },
    Racing { racers: HashMap<u32, RacerProgress>, next_place: u32 },
}

impl GameHub {
    fn new() -> Self {
        Self { state: State::Idle }
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.state, State::Idle)
    }

    /// Begins the ready handshake for a new game.
    fn start(&mut self, config: GameConfig) -> HubEffect {
        self.state = State::Starting { ready: HashSet::new() };
        HubEffect::Broadcast(ServerMessage::GameStarting { config })
    }

    /// Records that `id` is ready. Once every id in `participants` has
    /// called this, the race begins.
    fn client_ready(&mut self, id: u32, participants: &HashSet<u32>) -> HubEffect {
        let State::Starting { ready, .. } = &mut self.state else {
            return HubEffect::None;
        };
        ready.insert(id);
        if participants.is_subset(ready) { self.begin_race() } else { HubEffect::None }
    }

    /// Forces the race to begin even if some clients never confirmed
    /// ready (e.g. they've stalled or dropped). Idempotent once begun.
    fn force_begin(&mut self) -> HubEffect {
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
    /// `finished`. `all_finished` reflects whether every id in
    /// `participants` has now finished; once true, the hub goes back to
    /// `Idle` so a new game (a "play again") can be started.
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
            self.state = State::Idle;
        }

        HubEffect::Broadcast(ServerMessage::RaceState { racers, all_finished })
    }

    /// Ends the race early (host-triggered), reporting whatever progress
    /// racers had made as final and returning the hub to `Idle`. A no-op
    /// unless a race is actually in progress.
    fn force_end(&mut self) -> HubEffect {
        let State::Racing { racers, .. } = &self.state else {
            return HubEffect::None;
        };
        let racers: Vec<RacerProgress> = racers.values().cloned().collect();
        self.state = State::Idle;
        HubEffect::Broadcast(ServerMessage::RaceState { racers, all_finished: true })
    }
}

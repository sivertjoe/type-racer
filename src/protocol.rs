use serde::{Deserialize, Serialize};

/// Messages a client can send to the server, newline-delimited JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    /// Must be the first message sent after connecting.
    Join { code: String, username: String },
    /// Sent once the client has confirmed the game type shown on its
    /// join-confirmation screen. Only accepted clients appear in `Roster`.
    Accept,
    /// Sent once a client has set up local state for a `GameStarting` and
    /// is ready to race.
    ReadyForGame,
    /// This client's own progress during a race.
    Progress(ClientProgress),
}

/// Messages the server can send to a client, newline-delimited JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    Welcome,
    Rejected { reason: String },
    /// The full, current list of connected usernames. Sent to everyone
    /// whenever someone joins or leaves.
    Roster { users: Vec<String> },
    /// A game is starting; clients should set up local state for `config`
    /// and reply with `ReadyForGame`.
    GameStarting { config: GameConfig },
    /// Every connected client has confirmed ready (or the ready timeout
    /// elapsed) - go!
    GameBegin,
    /// Broadcast whenever any racer's progress changes. `all_finished` is
    /// true once every currently-connected client has finished.
    RaceState { racers: Vec<RacerProgress>, all_finished: bool },
}

/// Setup data for one game type - whatever every client needs to get into
/// the same starting state. Adding a new game means adding a variant here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GameConfig {
    TypingRace { sentence: String },
}

/// A client's report of its own progress. The server trusts `finished` and
/// doesn't interpret `detail` - it just relays it, so new game types don't
/// need any server-side changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientProgress {
    pub finished: bool,
    pub detail: GameProgress,
}

/// One racer's progress, as broadcast to everyone in `RaceState`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RacerProgress {
    pub username: String,
    pub finished: bool,
    /// 1-based finish rank, assigned by the server as racers finish.
    pub place: Option<u32>,
    pub detail: GameProgress,
}

/// Game-specific progress payload. Each game type gets one variant; the
/// server never matches on this, only clients rendering a race screen do.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GameProgress {
    /// `elapsed_ms` is time since that client's own countdown ended (i.e.
    /// since it started accepting input), used to compute WPM.
    TypingRace { correct_chars: usize, elapsed_ms: u64 },
}

/// Sent by a client over UDP broadcast to find a host advertising `code`.
#[derive(Debug, Serialize, Deserialize)]
pub struct DiscoveryRequest {
    pub code: String,
}

/// The host's unicast reply to a matching `DiscoveryRequest`.
#[derive(Debug, Serialize, Deserialize)]
pub struct DiscoveryResponse {
    pub code: String,
}

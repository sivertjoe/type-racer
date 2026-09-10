use serde::{Deserialize, Serialize};

/// Messages a client can send to the server, newline-delimited JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    /// Must be the first message sent after connecting.
    Join { code: String, username: String },
    /// Sent only by the host's own (loopback) connection, the moment it
    /// leaves Setup having picked a game - this is what makes the lobby
    /// discoverable/joinable at all, and lets joining clients show what
    /// they're joining.
    HostChoseGame { label: String },
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
    /// `game_label` is `None` for the host's own connection (it joins
    /// before picking anything) and always `Some` for every other client,
    /// since they can only join once the host has already chosen.
    Welcome { game_label: Option<String> },
    Rejected { reason: String },
    /// The full, current list of connected usernames. Sent to everyone
    /// whenever someone joins or leaves.
    Roster { users: Vec<String> },
    /// A round is starting for this client and they're racing in it;
    /// clients should set up local state for `config` and reply with
    /// `ReadyForGame`. Used both for Single Game's one round and for each
    /// round of a Knockout tournament.
    GameStarting { config: GameConfig },
    /// Sent instead of `GameStarting` to a client who isn't competing in
    /// this round (eliminated from a knockout) - same sentence, so their
    /// screen can show the same race read-only, but they can't type. Also
    /// expected to reply with `ReadyForGame`.
    Spectating { sentence: String },
    /// Every racer in this round has confirmed ready (or the ready
    /// timeout elapsed) - go!
    GameBegin,
    /// Broadcast whenever any racer's progress changes. `all_finished` is
    /// true once every racer in this round has finished.
    RaceState { racers: Vec<RacerProgress>, all_finished: bool },
    /// A knockout round (or a best-of-3 finals game) just ended.
    RoundOver(RoundOverInfo),
    /// The knockout tournament is fully decided, worst to best placement.
    TournamentOver { standings: Vec<String> },
}

/// Setup data for one game type - whatever every client needs to get into
/// the same starting state. Adding a new game means adding a variant here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GameConfig {
    TypingRace { sentence: String },
    /// Never sent over the wire as a per-round `GameStarting.config` -
    /// only used locally by the host to pick which kind of game to start;
    /// the tournament then drives its own rounds with `TypingRace`.
    Knockout { pacing: Pacing },
}

/// How a knockout tournament advances between rounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pacing {
    /// The host presses a key to start each next round.
    HostPaced,
    /// Each next round starts on its own after a short pause.
    Auto,
}

/// Reported after a knockout round (or a best-of-3 finals game) ends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundOverInfo {
    /// Usernames cut this round. Empty during the best-of-3 finals, where
    /// nobody actually leaves - they just lose a game.
    pub eliminated: Vec<String>,
    /// Usernames still competing.
    pub remaining: Vec<String>,
    pub pacing: Pacing,
    /// True on the round that brought the field down to the final two.
    pub entering_finals: bool,
    /// Present once in the best-of-3 phase: each finalist's games won so
    /// far.
    pub finals_score: Option<Vec<(String, u32)>>,
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

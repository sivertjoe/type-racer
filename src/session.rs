use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, watch};

use crate::knockout::KnockoutServer;
use crate::protocol::{ClientMessage, GameConfig, Pacing, ServerMessage};
use crate::single_game::SingleGameServer;
use crate::waiting::{ClientHandle, Clients};

/// How long the server waits for every racer to confirm `ReadyForGame`
/// before starting the round anyway (in case one is stuck or has dropped).
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long an auto-paced knockout waits after a round ends before
/// starting the next one.
const AUTO_ADVANCE_DELAY: Duration = Duration::from_secs(4);

/// A state change the caller needs to act on: broadcast the same message
/// to everyone, or send different clients different messages (a knockout
/// round starting sends racers `GameStarting` but spectators `Spectating`).
pub enum HubEffect {
    None,
    Broadcast(ServerMessage),
    Targeted(HashMap<u32, ServerMessage>),
}

/// One phase of a connection's lifetime. `waiting.rs` never needs to know
/// which gamemode (or none at all) is active - it just hands every
/// post-join message to whichever `Session` currently sits behind
/// [`Server`]. Starting a game replaces the lobby with a fresh instance of
/// the gamemode's own struct; nobody reconnects, only which struct
/// interprets a connection's messages changes.
pub trait Session: Send {
    fn handle_message(&mut self, id: u32, msg: ClientMessage, clients: &mut HashMap<u32, ClientHandle>) -> HubEffect;

    /// Ready-timeout fallback: forces the current round to begin even if
    /// not everyone confirmed ready.
    fn force_begin(&mut self) -> HubEffect {
        HubEffect::None
    }
    /// Host-triggered early end to the current round.
    fn force_end(&mut self) -> HubEffect {
        HubEffect::None
    }
    /// Host- (or auto-pacing-) triggered advance to the next round of a
    /// multi-round gamemode.
    fn continue_round(&mut self) -> HubEffect {
        HubEffect::None
    }
    /// Only the lobby accepts starting a new game.
    fn accepts_start(&self) -> bool {
        false
    }
    /// True once this gamemode has concluded and control should hand back
    /// to the lobby.
    fn concluded(&self) -> bool {
        false
    }
}

/// The join protocol: not a game at all, just tracks who's accepted so
/// far. It's genuinely stateless - the roster itself lives in `clients`,
/// which every phase can see - so there's only ever one `Lobby`.
pub struct Lobby;

impl Session for Lobby {
    /// `Accept` is handled directly by `waiting.rs` (connection admission,
    /// not lobby protocol) rather than here - see its comment there.
    fn handle_message(&mut self, _id: u32, _msg: ClientMessage, _clients: &mut HashMap<u32, ClientHandle>) -> HubEffect {
        HubEffect::None
    }

    fn accepts_start(&self) -> bool {
        true
    }
}

/// Sends every client the current list of *accepted* usernames (clients
/// still on their join-confirmation screen aren't included yet).
pub fn roster_effect(clients: &HashMap<u32, ClientHandle>) -> HubEffect {
    let mut accepted: Vec<&ClientHandle> = clients.values().filter(|c| c.accepted).collect();
    accepted.sort_by_key(|c| c.id);
    let users: Vec<String> = accepted.into_iter().map(|c| c.username.clone()).collect();
    HubEffect::Broadcast(ServerMessage::Roster { users })
}

/// The current session (the lobby, or a running gamemode) plus what's
/// needed to transition between them. `waiting.rs`'s per-connection tasks
/// call into this for every message after the initial `Join`.
pub struct Server {
    session: Mutex<Box<dyn Session>>,
    /// Set once the host picks a game in Setup - gates discovery/joining.
    /// Lives here rather than in `Lobby` since it's about connection
    /// admission, not lobby protocol state, and needs to survive the
    /// lobby being replaced by a gamemode.
    pub chosen_game: watch::Sender<Option<String>>,
}

pub type SharedServer = Arc<Server>;

impl Server {
    pub fn new(chosen_game: watch::Sender<Option<String>>) -> SharedServer {
        Arc::new(Server { session: Mutex::new(Box::new(Lobby)), chosen_game })
    }

    /// Routes one post-join message to whichever session is active, then
    /// hands control back to the lobby if that just concluded a game.
    pub async fn dispatch(&self, id: u32, msg: ClientMessage, clients: &Clients) -> HubEffect {
        let mut session = self.session.lock().await;
        let effect = {
            let mut guard = clients.lock().await;
            session.handle_message(id, msg, &mut guard)
        };
        self.return_to_lobby_if_concluded(&mut session);
        effect
    }

    /// Starts a new game (Single Game or a Knockout tournament) by
    /// constructing a fresh instance of that gamemode's own struct and
    /// putting it where the lobby was. A no-op unless the lobby is the
    /// current session (guards against e.g. a double Enter-press
    /// restarting an already-starting game), or - for Knockout - fewer
    /// than two players have accepted.
    pub async fn start(&self, config: GameConfig, clients: &Clients, stop_listening: &watch::Sender<bool>) -> HubEffect {
        let _ = stop_listening.send(true);

        let mut session = self.session.lock().await;
        if !session.accepts_start() {
            return HubEffect::None;
        }

        match config {
            GameConfig::TypingRace { sentence } => {
                let mut server = SingleGameServer::new();
                let effect = server.start(sentence);
                *session = Box::new(server);
                effect
            }
            GameConfig::Knockout { pacing } => {
                let participants = accepted_participants(clients).await;
                if participants.len() < 2 {
                    return HubEffect::None;
                }
                let mut server = KnockoutServer::new(pacing, participants);
                let effect = server.begin_round();
                *session = Box::new(server);
                effect
            }
        }
    }

    pub async fn force_begin(&self) -> HubEffect {
        let mut session = self.session.lock().await;
        let effect = session.force_begin();
        self.return_to_lobby_if_concluded(&mut session);
        effect
    }

    pub async fn force_end(&self) -> HubEffect {
        let mut session = self.session.lock().await;
        let effect = session.force_end();
        self.return_to_lobby_if_concluded(&mut session);
        effect
    }

    pub async fn continue_round(&self) -> HubEffect {
        let mut session = self.session.lock().await;
        let effect = session.continue_round();
        self.return_to_lobby_if_concluded(&mut session);
        effect
    }

    fn return_to_lobby_if_concluded(&self, session: &mut Box<dyn Session>) {
        if session.concluded() {
            *session = Box::new(Lobby);
        }
    }
}

/// The set of client ids currently accepted (past their join-confirmation
/// screen), with their usernames - what "everyone playing" means for
/// starting a game.
pub async fn accepted_participants(clients: &Clients) -> HashMap<u32, String> {
    clients.lock().await.values().filter(|c| c.accepted).map(|c| (c.id, c.username.clone())).collect()
}

/// Applies a `HubEffect`: broadcasts or targets messages as needed, and -
/// if a knockout round just ended in auto-pacing - schedules the next
/// round to start on its own after [`AUTO_ADVANCE_DELAY`].
///
/// Boxed because auto-pacing makes this self-recursive (each round-over
/// can schedule the next round's auto-advance, which calls back in here)
/// - the compiler can't size an `async fn` cycle like that without an
/// indirection to break it.
pub fn apply_effect<'a>(clients: &'a Clients, server: &'a SharedServer, effect: HubEffect) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        match effect {
            HubEffect::None => {}
            HubEffect::Broadcast(msg) => {
                let auto_advance = matches!(&msg, ServerMessage::RoundOver(info) if info.pacing == Pacing::Auto);
                broadcast(&*clients.lock().await, msg);
                if auto_advance {
                    let clients = clients.clone();
                    let server = server.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(AUTO_ADVANCE_DELAY).await;
                        let effect = server.continue_round().await;
                        apply_effect(&clients, &server, effect).await;
                    });
                }
            }
            HubEffect::Targeted(per_client) => {
                let guard = clients.lock().await;
                for (id, msg) in per_client {
                    if let Some(client) = guard.get(&id) {
                        let _ = client.outbox.send(msg);
                    }
                }
            }
        }
    })
}

pub fn schedule_ready_timeout(clients: Clients, server: SharedServer) {
    tokio::spawn(async move {
        tokio::time::sleep(READY_TIMEOUT).await;
        let effect = server.force_begin().await;
        apply_effect(&clients, &server, effect).await;
    });
}

/// Sends `msg` to every connected client.
pub fn broadcast(clients: &HashMap<u32, ClientHandle>, msg: ServerMessage) {
    for client in clients.values() {
        let _ = client.outbox.send(msg.clone());
    }
}

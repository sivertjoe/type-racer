use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Mutex, mpsc, watch};

use crate::protocol::{ClientMessage, DiscoveryRequest, DiscoveryResponse, ServerMessage};
use crate::session::{HubEffect, SharedServer, apply_effect, roster_effect};

/// Port the server listens on for game connections.
pub const PORT: u16 = 7878;

/// Port the server listens on for LAN discovery broadcasts.
pub const DISCOVERY_PORT: u16 = 7879;

/// A client the server has accepted into the lobby. This - not any
/// particular gamemode - is the "joining protocol" state: who's
/// connected, from where, under what name, and whether they've accepted.
/// It's shared across every phase of the connection's life; only the
/// `Session` that interprets further messages changes.
pub struct ClientHandle {
    pub id: u32,
    pub addr: SocketAddr,
    pub username: String,
    /// Set once the client confirms `Accept`; only accepted clients show
    /// up in the broadcast `Roster`.
    pub accepted: bool,
    /// Send a message here to have it delivered to this client.
    pub outbox: mpsc::UnboundedSender<ServerMessage>,
}

/// Live client list, shared with whatever else needs to see who's connected.
pub type Clients = Arc<Mutex<HashMap<u32, ClientHandle>>>;

/// Reported up to the caller as clients join and leave the lobby.
#[derive(Debug)]
pub enum LobbyEvent {
    ClientJoined { id: u32, addr: SocketAddr, username: String },
    ClientLeft { id: u32, addr: SocketAddr },
}

/// Generates a random 3-letter join code (e.g. "QXR").
pub fn generate_code() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    (0..3).map(|_| rng.random_range(b'A'..=b'Z') as char).collect()
}

/// Binds `PORT` and accepts connections until `stop` is set to `true`
/// (set once the host starts a game). Each connection must send a `Join`
/// with the correct code as its first message; on success it's added to
/// `clients`, everyone gets an updated `Roster`, and `events` gets a
/// `ClientJoined`. Purely transport from here on - every message after
/// `Join` is either handled locally (also transport/admission concerns:
/// `HostChoseGame`) or handed to `server.dispatch()`, which owns all
/// actual game/lobby protocol logic.
pub async fn listen(
    code: String,
    clients: Clients,
    server: SharedServer,
    events: mpsc::UnboundedSender<LobbyEvent>,
    mut stop: watch::Receiver<bool>,
) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", PORT))
        .await
        .context("failed to bind server listener")?;

    let mut next_id: u32 = 0;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, addr) = accepted.context("accept failed")?;
                let id = next_id;
                next_id += 1;

                tokio::spawn(handle_connection(id, addr, stream, code.clone(), clients.clone(), server.clone(), events.clone()));
            }
            result = stop.changed() => {
                // An error means the sender was dropped (e.g. the app is
                // shutting down) without ever sending `true` - treat that
                // as "stop" too, or this spins forever re-polling an
                // already-closed channel.
                if result.is_err() || *stop.borrow() {
                    return Ok(());
                }
            }
        }
    }
}

/// Listens for UDP discovery broadcasts and replies to any that carry a
/// matching `code`, until `stop` is set to `true`. Stays silent until the
/// host has picked a game in Setup - clients shouldn't be able to find,
/// and so join, a lobby the host hasn't actually opened yet.
pub async fn respond_to_discovery(code: String, mut stop: watch::Receiver<bool>, server: SharedServer) -> Result<()> {
    let socket = UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT))
        .await
        .context("failed to bind discovery socket")?;
    let chosen_game = server.chosen_game.subscribe();

    let mut buf = [0u8; 256];
    loop {
        tokio::select! {
            received = socket.recv_from(&mut buf) => {
                let (len, addr) = received.context("discovery recv failed")?;
                let Ok(request) = serde_json::from_slice::<DiscoveryRequest>(&buf[..len]) else {
                    continue;
                };
                if chosen_game.borrow().is_some()
                    && request.code.eq_ignore_ascii_case(&code)
                    && let Ok(json) = serde_json::to_vec(&DiscoveryResponse { code: code.clone() })
                {
                    let _ = socket.send_to(&json, addr).await;
                }
            }
            result = stop.changed() => {
                if result.is_err() || *stop.borrow() {
                    return Ok(());
                }
            }
        }
    }
}

/// Broadcasts `code` on the LAN and waits for a host to answer, retrying
/// for a few seconds since UDP broadcasts can be dropped. Returns the
/// host's address on success.
pub async fn find_host(code: &str) -> Result<SocketAddr> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).await.context("failed to bind discovery client socket")?;
    socket.set_broadcast(true).context("failed to enable broadcast")?;

    let request = serde_json::to_vec(&DiscoveryRequest { code: code.to_string() })
        .context("failed to encode discovery request")?;

    let mut buf = [0u8; 256];
    for _ in 0..10 {
        socket
            .send_to(&request, ("255.255.255.255", DISCOVERY_PORT))
            .await
            .context("failed to send discovery broadcast")?;

        if let Ok(Ok((len, addr))) = tokio::time::timeout(Duration::from_millis(500), socket.recv_from(&mut buf)).await
            && let Ok(response) = serde_json::from_slice::<DiscoveryResponse>(&buf[..len])
            && response.code.eq_ignore_ascii_case(code)
        {
            return Ok(addr);
        }
    }

    Err(eyre!("no host found for code {code} (waited 5s)"))
}

/// Owns one client's socket for its whole lifetime: does the join
/// handshake, registers it in `clients`, then pumps its outbox to the
/// socket and forwards every incoming message to `server` until it
/// disconnects.
async fn handle_connection(
    id: u32,
    addr: SocketAddr,
    stream: TcpStream,
    expected_code: String,
    clients: Clients,
    server: SharedServer,
    events: mpsc::UnboundedSender<LobbyEvent>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    let username = match lines.next_line().await {
        Ok(Some(line)) => match serde_json::from_str::<ClientMessage>(&line) {
            Ok(ClientMessage::Join { code, username }) if code.eq_ignore_ascii_case(&expected_code) => Some(username),
            _ => None,
        },
        _ => None,
    };

    let Some(username) = username else {
        let reject = ServerMessage::Rejected { reason: "bad code".into() };
        if let Ok(json) = serde_json::to_string(&reject) {
            let _ = write_half.write_all(format!("{json}\n").as_bytes()).await;
        }
        return;
    };

    // The discovery gate is the primary way this is avoided (a client
    // can't even find the host yet), but enforce it here too in case
    // someone connects with an address they already had cached. The
    // host's own (loopback) connection is exempt - it connects before
    // picking anything.
    let game_label = server.chosen_game.borrow().clone();
    if !addr.ip().is_loopback() && game_label.is_none() {
        let reject = ServerMessage::Rejected { reason: "the host hasn't chosen a game yet - try again in a moment".into() };
        if let Ok(json) = serde_json::to_string(&reject) {
            let _ = write_half.write_all(format!("{json}\n").as_bytes()).await;
        }
        return;
    }

    let (outbox_tx, mut outbox_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let username = {
        let mut guard = clients.lock().await;
        let username = dedupe_username(&guard, username);
        guard.insert(id, ClientHandle { id, addr, username: username.clone(), accepted: false, outbox: outbox_tx });
        // Not broadcast yet: this client isn't accepted, so the roster
        // hasn't actually changed for anyone.
        username
    };
    let _ = events.send(LobbyEvent::ClientJoined { id, addr, username });

    if let Ok(json) = serde_json::to_string(&ServerMessage::Welcome { game_label }) {
        let _ = write_half.write_all(format!("{json}\n").as_bytes()).await;
    }

    loop {
        tokio::select! {
            msg = outbox_rx.recv() => {
                let Some(msg) = msg else { break };
                let Ok(json) = serde_json::to_string(&msg) else { continue };
                if write_half.write_all(format!("{json}\n").as_bytes()).await.is_err() {
                    break;
                }
            }
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        let Ok(msg) = serde_json::from_str::<ClientMessage>(&line) else { continue };
                        match msg {
                            ClientMessage::Join { .. } => {} // already joined
                            // Connection admission, not lobby/game protocol
                            // - handled here rather than by a `Session`.
                            ClientMessage::HostChoseGame { label } => {
                                if addr.ip().is_loopback() {
                                    let _ = server.chosen_game.send(Some(label));
                                }
                            }
                            // Also connection admission: whether a client
                            // counts towards the participant set must not
                            // depend on which `Session` happens to be
                            // active when this arrives. Solo mode fires
                            // `Accept` and `HostCommand::Start` back to
                            // back with no delay between them, so the
                            // lobby can already have been replaced by a
                            // gamemode session (which doesn't understand
                            // `Accept`) by the time this is dispatched -
                            // silently dropping it here left the host
                            // permanently excluded from `participants`.
                            ClientMessage::Accept => {
                                let mut guard = clients.lock().await;
                                if let Some(client) = guard.get_mut(&id) {
                                    client.accepted = true;
                                }
                                if let HubEffect::Broadcast(msg) = roster_effect(&guard) {
                                    crate::session::broadcast(&guard, msg);
                                }
                            }
                            other => {
                                let effect = server.dispatch(id, other, &clients).await;
                                apply_effect(&clients, &server, effect).await;
                            }
                        }
                    }
                    _ => break,
                }
            }
        }
    }

    let mut guard = clients.lock().await;
    guard.remove(&id);
    if let HubEffect::Broadcast(msg) = roster_effect(&guard) {
        crate::session::broadcast(&guard, msg);
    }
    drop(guard);
    let _ = events.send(LobbyEvent::ClientLeft { id, addr });
}

/// If `username` is already taken by another connected client, appends
/// " (1)", " (2)", etc. until it's unique.
fn dedupe_username(clients: &HashMap<u32, ClientHandle>, username: String) -> String {
    let taken: std::collections::HashSet<&str> = clients.values().map(|c| c.username.as_str()).collect();
    if !taken.contains(username.as_str()) {
        return username;
    }
    (1..).map(|n| format!("{username} ({n})")).find(|candidate| !taken.contains(candidate.as_str())).unwrap()
}

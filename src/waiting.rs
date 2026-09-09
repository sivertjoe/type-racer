use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Mutex, mpsc, watch};

use crate::game_hub::{HubEffect, SharedGameHub};
use crate::protocol::{ClientMessage, DiscoveryRequest, DiscoveryResponse, GameConfig, ServerMessage};

/// How long the server waits for every client to confirm `ReadyForGame`
/// before starting the race anyway (in case one is stuck or has dropped).
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Port the server listens on for game connections.
pub const PORT: u16 = 7878;

/// Port the server listens on for LAN discovery broadcasts.
pub const DISCOVERY_PORT: u16 = 7879;

/// A client the server has accepted into the lobby.
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
/// (which [`start_game`] does once the host starts a game - see its
/// docs for why). Each connection must send a `Join` with the correct
/// code as its first message; on success it's added to `clients`,
/// everyone gets an updated `Roster`, and `events` gets a `ClientJoined`.
pub async fn listen(
    code: String,
    clients: Clients,
    hub: SharedGameHub,
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

                tokio::spawn(handle_connection(id, addr, stream, code.clone(), clients.clone(), hub.clone(), events.clone()));
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

/// Starts a new game: stops accepting new connections and discovery
/// requests (a client joining mid-race would never get `GameStarting`,
/// yet would still count toward the "has everyone finished" check
/// forever), broadcasts `GameStarting`, and once every connected client
/// has confirmed ready (or [`READY_TIMEOUT`] elapses), broadcasts
/// `GameBegin`.
pub async fn start_game(clients: &Clients, hub: &SharedGameHub, config: GameConfig, stop_listening: &watch::Sender<bool>) {
    let _ = stop_listening.send(true);

    if let HubEffect::Broadcast(msg) = hub.lock().await.start(config) {
        broadcast(&*clients.lock().await, msg);
    }

    let clients = clients.clone();
    let hub = hub.clone();
    tokio::spawn(async move {
        tokio::time::sleep(READY_TIMEOUT).await;
        if let HubEffect::Broadcast(msg) = hub.lock().await.force_begin() {
            broadcast(&*clients.lock().await, msg);
        }
    });
}

/// Host-triggered early end to the current race: reports whatever
/// progress racers had made as final. A no-op if no race is in progress.
pub async fn force_end_race(clients: &Clients, hub: &SharedGameHub) {
    if let HubEffect::Broadcast(msg) = hub.lock().await.force_end() {
        broadcast(&*clients.lock().await, msg);
    }
}

/// Listens for UDP discovery broadcasts and replies to any that carry a
/// matching `code`, until `stop` is set to `true`.
pub async fn respond_to_discovery(code: String, mut stop: watch::Receiver<bool>) -> Result<()> {
    let socket = UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT))
        .await
        .context("failed to bind discovery socket")?;

    let mut buf = [0u8; 256];
    loop {
        tokio::select! {
            received = socket.recv_from(&mut buf) => {
                let (len, addr) = received.context("discovery recv failed")?;
                let Ok(request) = serde_json::from_slice::<DiscoveryRequest>(&buf[..len]) else {
                    continue;
                };
                if request.code.eq_ignore_ascii_case(&code)
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
/// socket until it disconnects (or a message arrives, once gameplay
/// messages exist).
async fn handle_connection(
    id: u32,
    addr: SocketAddr,
    stream: TcpStream,
    expected_code: String,
    clients: Clients,
    hub: SharedGameHub,
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

    let (outbox_tx, mut outbox_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let username = {
        let mut guard = clients.lock().await;
        let username = dedupe_username(&guard, username);
        guard.insert(id, ClientHandle { id, addr, username: username.clone(), accepted: false, outbox: outbox_tx });
        // Not broadcast yet: this client isn't accepted, so the roster
        // hasn't actually changed for anyone.
        username
    };
    let _ = events.send(LobbyEvent::ClientJoined { id, addr, username: username.clone() });

    if let Ok(json) = serde_json::to_string(&ServerMessage::Welcome) {
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
                        let effect = match msg {
                            ClientMessage::Join { .. } => continue, // already joined
                            ClientMessage::Accept => {
                                let mut guard = clients.lock().await;
                                if let Some(client) = guard.get_mut(&id) {
                                    client.accepted = true;
                                }
                                broadcast_roster(&guard);
                                continue;
                            }
                            ClientMessage::ReadyForGame => {
                                let connected: HashSet<u32> = clients.lock().await.keys().copied().collect();
                                hub.lock().await.client_ready(id, &connected)
                            }
                            ClientMessage::Progress(progress) => {
                                let connected: HashSet<u32> = clients.lock().await.keys().copied().collect();
                                hub.lock().await.client_progress(id, username.clone(), progress, &connected)
                            }
                        };
                        if let HubEffect::Broadcast(msg) = effect {
                            broadcast(&*clients.lock().await, msg);
                        }
                    }
                    _ => break,
                }
            }
        }
    }

    {
        let mut guard = clients.lock().await;
        guard.remove(&id);
        broadcast_roster(&guard);
    }
    let _ = events.send(LobbyEvent::ClientLeft { id, addr });
}

/// If `username` is already taken by another connected client, appends
/// " (1)", " (2)", etc. until it's unique.
fn dedupe_username(clients: &HashMap<u32, ClientHandle>, username: String) -> String {
    let taken: HashSet<&str> = clients.values().map(|c| c.username.as_str()).collect();
    if !taken.contains(username.as_str()) {
        return username;
    }
    (1..).map(|n| format!("{username} ({n})")).find(|candidate| !taken.contains(candidate.as_str())).unwrap()
}

/// Sends every client the current list of *accepted* usernames (clients
/// still on their join-confirmation screen aren't included yet). Must be
/// called with the clients lock already held so the roster reflects the
/// change that triggered it.
fn broadcast_roster(clients: &HashMap<u32, ClientHandle>) {
    let mut accepted: Vec<&ClientHandle> = clients.values().filter(|c| c.accepted).collect();
    accepted.sort_by_key(|c| c.id);
    let users: Vec<String> = accepted.into_iter().map(|c| c.username.clone()).collect();
    broadcast(clients, ServerMessage::Roster { users });
}

/// Sends `msg` to every connected client.
fn broadcast(clients: &HashMap<u32, ClientHandle>, msg: ServerMessage) {
    for client in clients.values() {
        let _ = client.outbox.send(msg.clone());
    }
}

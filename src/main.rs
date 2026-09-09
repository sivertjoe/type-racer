use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};
use crossterm::event::{self, KeyCode};
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{Block, List, ListItem, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};

mod game_hub;
mod protocol;
mod setup;
mod typing_race;
mod waiting;

use protocol::{ClientMessage, GameConfig, ServerMessage};

enum Mode {
    /// No arguments: just play by yourself, skipping straight past game
    /// selection and the lobby wait.
    Solo,
    Host,
    Connect { code: String },
}

fn parse_mode() -> Result<Mode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => Ok(Mode::Solo),
        [flag] if flag == "--host" => Ok(Mode::Host),
        [flag, code] if flag == "--connect" => Ok(Mode::Connect { code: code.clone() }),
        _ => Err(eyre!("usage: type-racer [--host | --connect <code>]")),
    }
}

/// Falls back through the usual OS environment variables for the current
/// login name, since we don't have a name-entry screen yet.
fn detect_username() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "player".to_string())
}

/// Connects to `ip:PORT`, sends the join code and username, and waits for
/// the server's `Welcome`. On success, spawns a reader task (forwarding
/// every subsequent `ServerMessage` onto the returned receiver) and a
/// writer task (accepting `ClientMessage`s on the returned sender and
/// writing them to the socket).
async fn join_server(
    ip: &str,
    code: String,
    username: String,
) -> Result<(mpsc::UnboundedSender<ClientMessage>, mpsc::UnboundedReceiver<ServerMessage>)> {
    let stream = TcpStream::connect((ip, waiting::PORT))
        .await
        .context("failed to connect to server")?;

    let (read_half, mut write_half) = stream.into_split();
    let join = serde_json::to_string(&ClientMessage::Join { code, username }).context("failed to encode join message")?;
    write_half
        .write_all(format!("{join}\n").as_bytes())
        .await
        .context("failed to send join message")?;

    let mut lines = BufReader::new(read_half).lines();
    match lines.next_line().await.context("failed to read server reply")? {
        Some(line) => match serde_json::from_str::<ServerMessage>(&line) {
            Ok(ServerMessage::Welcome) => {}
            Ok(ServerMessage::Rejected { reason }) => return Err(eyre!("server rejected join: {reason}")),
            Ok(other) => return Err(eyre!("unexpected first reply from server: {other:?}")),
            Err(err) => return Err(eyre!("unexpected reply from server: {err}")),
        },
        None => return Err(eyre!("server closed the connection during join")),
    }

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ClientMessage>();
    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let Ok(json) = serde_json::to_string(&msg) else { continue };
            if write_half.write_all(format!("{json}\n").as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let (in_tx, in_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if let Ok(msg) = serde_json::from_str::<ServerMessage>(&line)
                        && in_tx.send(msg).is_err()
                    {
                        break;
                    }
                }
                _ => break,
            }
        }
    });

    Ok((out_tx, in_rx))
}

/// Host-only actions that need the server's authoritative state (the
/// `GameHub`), sent through `App::host_tx` to the task that owns it.
enum HostCommand {
    Start(GameConfig),
    EndRace,
}

enum Screen {
    /// Host only: picking the game type before the lobby opens.
    Setup { menu: setup::GameKindMenu },
    /// Client only: confirming the game type the host picked.
    JoinConfirm { kind: setup::GameKind },
    Lobby { kind: setup::GameKind },
    Race(typing_race::RaceScreen),
}

struct App {
    should_quit: bool,
    is_host: bool,
    code: String,
    users: Vec<String>,
    /// Only present for the host: sends commands here that only the host
    /// may issue (starting or ending a game).
    host_tx: Option<mpsc::UnboundedSender<HostCommand>>,
    net_tx: mpsc::UnboundedSender<ClientMessage>,
    net_rx: mpsc::UnboundedReceiver<ServerMessage>,
    screen: Screen,
}

impl App {
    fn new(
        is_host: bool,
        code: String,
        auto_start: bool,
        host_tx: Option<mpsc::UnboundedSender<HostCommand>>,
        net_tx: mpsc::UnboundedSender<ClientMessage>,
        net_rx: mpsc::UnboundedReceiver<ServerMessage>,
    ) -> Self {
        // There's only one game kind today, so the client's "confirmation"
        // is cosmetic - once the host can actually pick among several,
        // the chosen kind should come from the server instead of this
        // hardcoded default.
        let screen = if auto_start {
            Screen::Lobby { kind: setup::GameKind::SingleGame }
        } else if is_host {
            Screen::Setup { menu: setup::GameKindMenu::new() }
        } else {
            Screen::JoinConfirm { kind: setup::GameKind::SingleGame }
        };
        let app = Self { should_quit: false, is_host, code, users: Vec::new(), host_tx, net_tx, net_rx, screen };

        // Solo mode: nobody to wait for, so skip straight past game
        // selection and the lobby - accept immediately and kick the race
        // off as soon as the server round-trips back.
        if auto_start {
            let _ = app.net_tx.send(ClientMessage::Accept);
            if let Some(tx) = &app.host_tx {
                let _ = tx.send(HostCommand::Start(setup::GameKind::SingleGame.config()));
            }
        }

        app
    }

    fn update(&mut self) -> Result<()> {
        loop {
            match self.net_rx.try_recv() {
                Ok(msg) => self.handle_server_message(msg),
                Err(mpsc::error::TryRecvError::Empty) => break,
                // The reader task only ever exits when the socket closed,
                // which means the server (host process) is gone - nothing
                // to do but leave too.
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.should_quit = true;
                    break;
                }
            }
        }

        if event::poll(Duration::from_millis(250)).context("event poll failed")?
            && let Some(key) = event::read().context("event read failed")?.as_key_press_event()
        {
            self.handle_key(key.code);
        }
        Ok(())
    }

    fn handle_server_message(&mut self, msg: ServerMessage) {
        match msg {
            ServerMessage::Roster { users } => self.users = users,
            ServerMessage::GameStarting { config } => {
                match config {
                    GameConfig::TypingRace { sentence } => {
                        self.screen = Screen::Race(typing_race::RaceScreen::new(sentence));
                    }
                }
                let _ = self.net_tx.send(ClientMessage::ReadyForGame);
            }
            ServerMessage::GameBegin => {
                if let Screen::Race(race) = &mut self.screen {
                    race.on_game_begin();
                }
            }
            ServerMessage::RaceState { racers, all_finished } => {
                if let Screen::Race(race) = &mut self.screen {
                    race.set_racers(racers, all_finished);
                }
            }
            ServerMessage::Welcome | ServerMessage::Rejected { .. } => {}
        }
    }

    /// Keybinds depend on the screen: everywhere except while racing, 'q'
    /// quits; while racing 'q' is a letter in the sentence, so quitting
    /// there is Esc instead.
    fn handle_key(&mut self, code: KeyCode) {
        match &mut self.screen {
            Screen::Setup { menu } => match code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Up => menu.move_up(),
                KeyCode::Down => menu.move_down(),
                KeyCode::Enter => {
                    self.screen = Screen::Lobby { kind: menu.selected() };
                    let _ = self.net_tx.send(ClientMessage::Accept);
                }
                _ => {}
            },
            Screen::JoinConfirm { kind } => match code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Enter => {
                    self.screen = Screen::Lobby { kind: *kind };
                    let _ = self.net_tx.send(ClientMessage::Accept);
                }
                _ => {}
            },
            Screen::Lobby { kind } => match code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Enter => {
                    if let Some(tx) = &self.host_tx {
                        let _ = tx.send(HostCommand::Start(kind.config()));
                    }
                }
                _ => {}
            },
            Screen::Race(race) => match code {
                KeyCode::Esc => self.should_quit = true,
                KeyCode::Enter if self.is_host && !race.is_over() => {
                    if let Some(tx) = &self.host_tx {
                        let _ = tx.send(HostCommand::EndRace);
                    }
                }
                KeyCode::Char('r') if self.is_host && race.is_over() => {
                    if let Some(tx) = &self.host_tx {
                        let _ = tx.send(HostCommand::Start(setup::GameKind::SingleGame.config()));
                    }
                }
                KeyCode::Char(c) => {
                    if let Some(progress) = race.handle_char(c) {
                        let _ = self.net_tx.send(ClientMessage::Progress(progress));
                    }
                }
                _ => {}
            },
        }
    }
}

/// Binds the listener and discovery responder and starts the task that
/// turns `HostCommand`s into actual server-side effects. Shared by
/// `--host` and the no-args solo mode, which are identical except for
/// whether the player has to pick a game type and wait in the lobby.
fn start_hosting(code: String) -> mpsc::UnboundedSender<HostCommand> {
    let clients: waiting::Clients = Arc::new(Mutex::new(HashMap::new()));
    let hub = game_hub::GameHub::new();
    // The roster (sent as a ServerMessage and rendered in the TUI) already
    // covers joins/leaves, so LobbyEvent is unused for now and just
    // dropped here. Printing it would corrupt the TUI's alternate screen,
    // so don't route it to stdout/stderr.
    let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
    // Flips to true once the first game starts, so the listener and
    // discovery responder stop taking on new (and now-unhelpable)
    // latecomers - including across a later "play again".
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);

    tokio::spawn(waiting::listen(code.clone(), clients.clone(), hub.clone(), events_tx, stop_rx.clone()));
    tokio::spawn(waiting::respond_to_discovery(code, stop_rx));

    let (host_tx, mut host_rx) = mpsc::unbounded_channel::<HostCommand>();
    tokio::spawn(async move {
        while let Some(cmd) = host_rx.recv().await {
            match cmd {
                HostCommand::Start(config) => waiting::start_game(&clients, &hub, config, &stop_tx).await,
                HostCommand::EndRace => waiting::force_end_race(&clients, &hub).await,
            }
        }
    });
    host_tx
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let mode = parse_mode()?;
    let username = detect_username();

    let (is_host, code, host_ip, host_tx, auto_start) = match mode {
        Mode::Solo => {
            let code = waiting::generate_code();
            let host_tx = start_hosting(code.clone());
            println!("starting a solo game...");
            (true, code, "127.0.0.1".to_string(), Some(host_tx), true)
        }
        Mode::Host => {
            let code = waiting::generate_code();
            let host_tx = start_hosting(code.clone());
            println!("hosting — join code: {code}");
            (true, code, "127.0.0.1".to_string(), Some(host_tx), false)
        }
        Mode::Connect { code } => {
            println!("looking for host with code {code}...");
            let addr = waiting::find_host(&code).await?;
            (false, code, addr.ip().to_string(), None, false)
        }
    };

    let (net_tx, net_rx) = join_server(&host_ip, code.clone(), username).await?;

    tokio::task::spawn_blocking(move || {
        ratatui::run(|terminal| run(terminal, is_host, code, auto_start, host_tx, net_tx, net_rx)).context("failed to run app")
    })
    .await
    .context("tui task panicked")?
}

fn run(
    terminal: &mut DefaultTerminal,
    is_host: bool,
    code: String,
    auto_start: bool,
    host_tx: Option<mpsc::UnboundedSender<HostCommand>>,
    net_tx: mpsc::UnboundedSender<ClientMessage>,
    net_rx: mpsc::UnboundedReceiver<ServerMessage>,
) -> Result<()> {
    let mut app = App::new(is_host, code, auto_start, host_tx, net_tx, net_rx);
    while !app.should_quit {
        terminal.draw(|frame| render(frame, &app))?;
        app.update()?;
    }
    Ok(())
}

fn render(frame: &mut Frame, app: &App) {
    match &app.screen {
        Screen::Setup { menu } => menu.render(frame, frame.area()),
        Screen::JoinConfirm { kind } => setup::render_join_confirm(frame, frame.area(), *kind),
        Screen::Lobby { .. } => render_lobby(frame, app),
        Screen::Race(race) => race.render(frame, frame.area(), app.is_host),
    }
}

fn render_lobby(frame: &mut Frame, app: &App) {
    let [players_area, hint_area] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());

    let items: Vec<ListItem> = app.users.iter().map(|u| ListItem::new(u.as_str())).collect();
    let list = List::new(items).block(Block::bordered().title(format!("Players — code: {}", app.code)));
    frame.render_widget(list, players_area);

    let hint = if app.is_host { "press ENTER to start, 'q' to quit" } else { "waiting for host to start... ('q' to quit)" };
    frame.render_widget(Paragraph::new(hint), hint_area);
}

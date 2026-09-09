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

mod knockout;
mod protocol;
mod session;
mod setup;
mod single_game;
mod typing_race;
mod waiting;
mod words;

use protocol::{ClientMessage, GameConfig, Pacing, RoundOverInfo, ServerMessage};

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
) -> Result<(Option<String>, mpsc::UnboundedSender<ClientMessage>, mpsc::UnboundedReceiver<ServerMessage>)> {
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
    let game_label = match lines.next_line().await.context("failed to read server reply")? {
        Some(line) => match serde_json::from_str::<ServerMessage>(&line) {
            Ok(ServerMessage::Welcome { game_label }) => game_label,
            Ok(ServerMessage::Rejected { reason }) => return Err(eyre!("server rejected join: {reason}")),
            Ok(other) => return Err(eyre!("unexpected first reply from server: {other:?}")),
            Err(err) => return Err(eyre!("unexpected reply from server: {err}")),
        },
        None => return Err(eyre!("server closed the connection during join")),
    };

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

    Ok((game_label, out_tx, in_rx))
}

/// Host-only actions that need the server's authoritative state, sent
/// through `App::host_tx` to the task that owns it.
enum HostCommand {
    Start(GameConfig),
    EndRace,
    /// Advance a knockout tournament to its next round.
    ContinueRound,
}

enum Screen {
    /// Host only: picking the game type before the lobby opens.
    Setup { menu: setup::GameKindMenu },
    /// Client only: waiting to confirm once the host has picked a game.
    /// `label` is always present in practice - a client can only reach
    /// this screen after the host has already announced its choice.
    JoinConfirm { label: String },
    Lobby { kind: setup::GameKind },
    /// Used both for Single Game's one race and for each round of a
    /// knockout tournament (racing or spectating).
    Race(typing_race::RaceScreen),
    /// A knockout round (or best-of-3 finals game) just ended.
    KnockoutRoundOver(RoundOverInfo),
    /// The knockout tournament is fully decided.
    KnockoutOver { standings: Vec<String> },
}

struct App {
    should_quit: bool,
    is_host: bool,
    code: String,
    users: Vec<String>,
    /// The kind last used to start a game - remembered so "play again"
    /// (both Single Game's and a knockout tournament's) replays the same
    /// kind without the host having to revisit Setup.
    last_kind: setup::GameKind,
    /// Only present for the host: sends commands here that only the host
    /// may issue (starting, ending, or advancing a game).
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
        joined_label: Option<String>,
        host_tx: Option<mpsc::UnboundedSender<HostCommand>>,
        net_tx: mpsc::UnboundedSender<ClientMessage>,
        net_rx: mpsc::UnboundedReceiver<ServerMessage>,
    ) -> Self {
        let screen = if auto_start {
            Screen::Lobby { kind: setup::GameKind::SingleGame }
        } else if is_host {
            Screen::Setup { menu: setup::GameKindMenu::new() }
        } else {
            Screen::JoinConfirm { label: joined_label.unwrap_or_else(|| "a game".to_string()) }
        };
        let app = Self {
            should_quit: false,
            is_host,
            code,
            users: Vec::new(),
            last_kind: setup::GameKind::SingleGame,
            host_tx,
            net_tx,
            net_rx,
            screen,
        };

        // Solo mode: nobody to wait for, so skip straight past game
        // selection and the lobby - accept immediately and kick the race
        // off as soon as the server round-trips back.
        if auto_start {
            let _ = app.net_tx.send(ClientMessage::HostChoseGame { label: setup::GameKind::SingleGame.label().to_string() });
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
                    // Never actually sent as a per-round config - Knockout
                    // is only ever a local `HostCommand::Start` argument;
                    // the tournament drives its own rounds with
                    // `TypingRace`. Kept here only so this match stays
                    // exhaustive as `GameConfig` grows.
                    GameConfig::Knockout { .. } => {}
                }
                let _ = self.net_tx.send(ClientMessage::ReadyForGame);
            }
            ServerMessage::Spectating { sentence } => {
                self.screen = Screen::Race(typing_race::RaceScreen::new_spectating(sentence));
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
            ServerMessage::RoundOver(info) => self.screen = Screen::KnockoutRoundOver(info),
            ServerMessage::TournamentOver { standings } => self.screen = Screen::KnockoutOver { standings },
            ServerMessage::Welcome { .. } | ServerMessage::Rejected { .. } => {}
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
                    let kind = menu.selected();
                    self.screen = Screen::Lobby { kind };
                    let _ = self.net_tx.send(ClientMessage::HostChoseGame { label: kind.label().to_string() });
                    let _ = self.net_tx.send(ClientMessage::Accept);
                }
                _ => {}
            },
            Screen::JoinConfirm { .. } => match code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Enter => {
                    self.screen = Screen::Lobby { kind: setup::GameKind::SingleGame }; // unused by non-hosts
                    let _ = self.net_tx.send(ClientMessage::Accept);
                }
                _ => {}
            },
            Screen::Lobby { kind } => match code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Enter => {
                    if let Some(tx) = &self.host_tx {
                        self.last_kind = *kind;
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
                        let _ = tx.send(HostCommand::Start(self.last_kind.config()));
                    }
                }
                KeyCode::Char(c) => {
                    if let Some(progress) = race.handle_char(c) {
                        let _ = self.net_tx.send(ClientMessage::Progress(progress));
                    }
                }
                _ => {}
            },
            Screen::KnockoutRoundOver(info) => match code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Enter if self.is_host && info.pacing == Pacing::HostPaced => {
                    if let Some(tx) = &self.host_tx {
                        let _ = tx.send(HostCommand::ContinueRound);
                    }
                }
                _ => {}
            },
            Screen::KnockoutOver { .. } => match code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Char('r') if self.is_host => {
                    if let Some(tx) = &self.host_tx {
                        let _ = tx.send(HostCommand::Start(self.last_kind.config()));
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
    // The roster (sent as a ServerMessage and rendered in the TUI) already
    // covers joins/leaves, so LobbyEvent is unused for now and just
    // dropped here. Printing it would corrupt the TUI's alternate screen,
    // so don't route it to stdout/stderr.
    let (events_tx, _events_rx) = tokio::sync::mpsc::unbounded_channel();
    // Flips to true once the first game starts, so the listener and
    // discovery responder stop taking on new (and now-unhelpable)
    // latecomers - including across a later "play again".
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    // Set once the host has picked a game in Setup - before that, the
    // lobby isn't discoverable or joinable at all.
    let (chosen_game_tx, _) = tokio::sync::watch::channel(None);
    let server = session::Server::new(chosen_game_tx);

    tokio::spawn(waiting::listen(code.clone(), clients.clone(), server.clone(), events_tx, stop_rx.clone()));
    tokio::spawn(waiting::respond_to_discovery(code, stop_rx, server.clone()));

    let (host_tx, mut host_rx) = mpsc::unbounded_channel::<HostCommand>();
    tokio::spawn(async move {
        while let Some(cmd) = host_rx.recv().await {
            match cmd {
                HostCommand::Start(config) => {
                    let effect = server.start(config, &clients, &stop_tx).await;
                    session::apply_effect(&clients, &server, effect).await;
                    session::schedule_ready_timeout(clients.clone(), server.clone());
                }
                HostCommand::EndRace => {
                    let effect = server.force_end().await;
                    session::apply_effect(&clients, &server, effect).await;
                }
                HostCommand::ContinueRound => {
                    let effect = server.continue_round().await;
                    session::apply_effect(&clients, &server, effect).await;
                    session::schedule_ready_timeout(clients.clone(), server.clone());
                }
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

    let (joined_label, net_tx, net_rx) = join_server(&host_ip, code.clone(), username).await?;

    tokio::task::spawn_blocking(move || {
        ratatui::run(|terminal| run(terminal, is_host, code, auto_start, joined_label, host_tx, net_tx, net_rx)).context("failed to run app")
    })
    .await
    .context("tui task panicked")?
}

fn run(
    terminal: &mut DefaultTerminal,
    is_host: bool,
    code: String,
    auto_start: bool,
    joined_label: Option<String>,
    host_tx: Option<mpsc::UnboundedSender<HostCommand>>,
    net_tx: mpsc::UnboundedSender<ClientMessage>,
    net_rx: mpsc::UnboundedReceiver<ServerMessage>,
) -> Result<()> {
    let mut app = App::new(is_host, code, auto_start, joined_label, host_tx, net_tx, net_rx);
    while !app.should_quit {
        terminal.draw(|frame| render(frame, &app))?;
        app.update()?;
    }
    Ok(())
}

fn render(frame: &mut Frame, app: &App) {
    match &app.screen {
        Screen::Setup { menu } => menu.render(frame, frame.area()),
        Screen::JoinConfirm { label } => setup::render_join_confirm(frame, frame.area(), label),
        Screen::Lobby { .. } => render_lobby(frame, app),
        Screen::Race(race) => race.render(frame, frame.area(), app.is_host),
        Screen::KnockoutRoundOver(info) => render_knockout_round_over(frame, app, info),
        Screen::KnockoutOver { standings } => render_knockout_over(frame, app, standings),
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

fn render_knockout_round_over(frame: &mut Frame, app: &App, info: &RoundOverInfo) {
    let [status_area, lists_area, hint_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());

    if let Some(score) = &info.finals_score {
        let score_text = score.iter().map(|(name, wins)| format!("{name}: {wins}")).collect::<Vec<_>>().join("  vs  ");
        frame.render_widget(Paragraph::new(format!("Finals! {score_text}")), status_area);
    } else if info.entering_finals {
        frame.render_widget(Paragraph::new("Down to the final two - best of 3 begins!"), status_area);
    } else {
        frame.render_widget(Paragraph::new("Round over"), status_area);
    }

    let mut lines: Vec<ListItem> = Vec::new();
    if !info.eliminated.is_empty() {
        lines.push(ListItem::new(format!("Eliminated: {}", info.eliminated.join(", "))));
    }
    lines.push(ListItem::new(format!("Still in: {}", info.remaining.join(", "))));
    frame.render_widget(List::new(lines).block(Block::bordered().title("Knockout")), lists_area);

    let hint = match (app.is_host, info.pacing) {
        (true, Pacing::HostPaced) => "ENTER for next round, 'q' to quit",
        (false, Pacing::HostPaced) => "waiting for host to continue... ('q' to quit)",
        (_, Pacing::Auto) => "next round starting automatically... ('q' to quit)",
    };
    frame.render_widget(Paragraph::new(hint), hint_area);
}

fn render_knockout_over(frame: &mut Frame, app: &App, standings: &[String]) {
    let [title_area, standings_area, hint_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());

    frame.render_widget(Paragraph::new("Tournament over!"), title_area);

    let items: Vec<ListItem> = standings.iter().enumerate().map(|(i, name)| ListItem::new(format!("{}. {name}", i + 1))).collect();
    frame.render_widget(List::new(items).block(Block::bordered().title("Final standings")), standings_area);

    let hint = if app.is_host { "R to play again, 'q' to quit" } else { "'q' to quit" };
    frame.render_widget(Paragraph::new(hint), hint_area);
}

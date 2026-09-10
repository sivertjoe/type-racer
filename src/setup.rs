use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, List, ListItem, Paragraph};

use type_racer_core::protocol::{GameConfig, Pacing};
use type_racer_core::words;

/// The kind of game a lobby is set up to play - and, for Knockout, how it
/// paces itself between rounds. Adding a new game means a new variant
/// here (plus a `config()` arm and its own screen/logic module), and a
/// new `TopEntry` if it should show up in the Setup menu;
/// `waiting.rs`/`session.rs` don't need to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameKind {
    SingleGame,
    Knockout(Pacing),
}

impl GameKind {
    pub fn label(self) -> &'static str {
        match self {
            GameKind::SingleGame => "Single Game (typing race)",
            GameKind::Knockout(Pacing::HostPaced) => "Knockout (host starts each round)",
            GameKind::Knockout(Pacing::Auto) => "Knockout (rounds auto-advance)",
        }
    }

    /// Builds a fresh `GameConfig` for this kind - called every time a
    /// game starts (including "play again"), so each race gets new random
    /// content.
    pub fn config(self) -> GameConfig {
        match self {
            GameKind::SingleGame => GameConfig::TypingRace { sentence: words::generate_sentence() },
            GameKind::Knockout(pacing) => GameConfig::Knockout { pacing },
        }
    }
}

/// A row in the top-level Setup menu. Distinct from `GameKind` because a
/// row like "Knockout" doesn't fully pick one on its own - it opens a
/// submenu of its options first.
#[derive(Clone, Copy)]
enum TopEntry {
    SingleGame,
    Knockout,
}

impl TopEntry {
    const ALL: &'static [TopEntry] = &[TopEntry::SingleGame, TopEntry::Knockout];

    fn label(self) -> &'static str {
        match self {
            TopEntry::SingleGame => "Single Game (typing race)",
            TopEntry::Knockout => "Knockout",
        }
    }

    fn has_options(self) -> bool {
        matches!(self, TopEntry::Knockout)
    }
}

const KNOCKOUT_PACINGS: &[Pacing] = &[Pacing::HostPaced, Pacing::Auto];

fn pacing_label(pacing: Pacing) -> &'static str {
    match pacing {
        Pacing::HostPaced => "host starts each round",
        Pacing::Auto => "rounds auto-advance",
    }
}

/// The host's game-type picker, shown before the lobby opens. A game with
/// no further choices (Single Game) confirms directly; one with options
/// (Knockout) opens a submenu - via right arrow, or Enter on that row -
/// to pick among them.
pub struct GameKindMenu {
    state: MenuState,
}

enum MenuState {
    Top { selected: usize },
    KnockoutOptions { selected: usize },
}

impl GameKindMenu {
    pub fn new() -> Self {
        Self { state: MenuState::Top { selected: 0 } }
    }

    pub fn move_up(&mut self) {
        match &mut self.state {
            MenuState::Top { selected } => *selected = selected.saturating_sub(1),
            MenuState::KnockoutOptions { selected } => *selected = selected.saturating_sub(1),
        }
    }

    pub fn move_down(&mut self) {
        match &mut self.state {
            MenuState::Top { selected } => *selected = (*selected + 1).min(TopEntry::ALL.len() - 1),
            MenuState::KnockoutOptions { selected } => *selected = (*selected + 1).min(KNOCKOUT_PACINGS.len() - 1),
        }
    }

    /// Drills into the current row's submenu, if it has one.
    pub fn move_right(&mut self) {
        if let MenuState::Top { selected } = self.state
            && TopEntry::ALL[selected].has_options()
        {
            self.state = MenuState::KnockoutOptions { selected: 0 };
        }
    }

    /// Backs out of a submenu, if in one.
    pub fn move_left(&mut self) {
        if matches!(self.state, MenuState::KnockoutOptions { .. }) {
            let knockout_index = TopEntry::ALL.iter().position(|e| e.has_options()).unwrap_or(0);
            self.state = MenuState::Top { selected: knockout_index };
        }
    }

    /// Confirms the current row. Returns the picked `GameKind`, or `None`
    /// if this just opened a submenu instead (a row with options, chosen
    /// from the top level) - the caller should keep showing the menu.
    pub fn confirm(&mut self) -> Option<GameKind> {
        match self.state {
            MenuState::Top { selected } => match TopEntry::ALL[selected] {
                TopEntry::SingleGame => Some(GameKind::SingleGame),
                TopEntry::Knockout => {
                    self.state = MenuState::KnockoutOptions { selected: 0 };
                    None
                }
            },
            MenuState::KnockoutOptions { selected } => Some(GameKind::Knockout(KNOCKOUT_PACINGS[selected])),
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        match &self.state {
            MenuState::Top { selected } => {
                let labels: Vec<String> =
                    TopEntry::ALL.iter().map(|e| if e.has_options() { format!("{} ›", e.label()) } else { e.label().to_string() }).collect();
                render_menu(frame, area, "Choose a game", &labels, *selected, "up/down to choose, → for options, ENTER to confirm, 'q' to quit");
            }
            MenuState::KnockoutOptions { selected } => {
                let labels: Vec<String> = KNOCKOUT_PACINGS.iter().map(|p| pacing_label(*p).to_string()).collect();
                render_menu(frame, area, "Knockout", &labels, *selected, "up/down to choose, ← back, ENTER to confirm, 'q' to quit");
            }
        }
    }
}

/// The client's join screen: shows the game the host already picked
/// (guaranteed by this point, since joining is gated on the host having
/// chosen one - see `waiting::respond_to_discovery`).
pub fn render_join_confirm(frame: &mut Frame, area: Rect, label: &str) {
    let [list_area, hint_area] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);
    let body = Paragraph::new(label).block(Block::bordered().title("Joining"));
    frame.render_widget(body, list_area);
    frame.render_widget(Paragraph::new("ENTER to join, 'q' to quit"), hint_area);
}

fn render_menu(frame: &mut Frame, area: Rect, title: &str, labels: &[String], selected: usize, hint: &str) {
    let [list_area, hint_area] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);

    let items: Vec<ListItem> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let style = if i == selected { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default() };
            ListItem::new(label.as_str()).style(style)
        })
        .collect();
    frame.render_widget(List::new(items).block(Block::bordered().title(title)), list_area);
    frame.render_widget(Paragraph::new(hint), hint_area);
}

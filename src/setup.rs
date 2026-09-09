use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, List, ListItem, Paragraph};

use crate::protocol::{GameConfig, Pacing};
use crate::typing_race;

/// The kind of game a lobby is set up to play - and, for Knockout, how it
/// paces itself between rounds. Adding a new game means a new variant
/// here (plus a `config()` arm and its own screen/logic module);
/// `waiting.rs` and `game_hub.rs` don't need to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameKind {
    SingleGame,
    Knockout(Pacing),
}

impl GameKind {
    pub const ALL: &'static [GameKind] = &[GameKind::SingleGame, GameKind::Knockout(Pacing::HostPaced), GameKind::Knockout(Pacing::Auto)];

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
            GameKind::SingleGame => GameConfig::TypingRace { sentence: typing_race::generate_sentence() },
            GameKind::Knockout(pacing) => GameConfig::Knockout { pacing },
        }
    }
}

/// The host's game-type picker, shown before the lobby opens.
pub struct GameKindMenu {
    selected: usize,
}

impl GameKindMenu {
    pub fn new() -> Self {
        Self { selected: 0 }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        self.selected = (self.selected + 1).min(GameKind::ALL.len() - 1);
    }

    pub fn selected(&self) -> GameKind {
        GameKind::ALL[self.selected]
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        render_options(frame, area, "Choose a game", GameKind::ALL, self.selected, "up/down to choose, ENTER to confirm, 'q' to quit");
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

fn render_options(frame: &mut Frame, area: Rect, title: &str, kinds: &[GameKind], selected: usize, hint: &str) {
    let [list_area, hint_area] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);

    let items: Vec<ListItem> = kinds
        .iter()
        .enumerate()
        .map(|(i, kind)| {
            let style = if i == selected { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default() };
            ListItem::new(kind.label()).style(style)
        })
        .collect();
    frame.render_widget(List::new(items).block(Block::bordered().title(title)), list_area);
    frame.render_widget(Paragraph::new(hint), hint_area);
}

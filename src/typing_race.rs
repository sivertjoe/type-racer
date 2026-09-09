use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Gauge, List, ListItem, Paragraph};

use crate::protocol::{ClientProgress, GameProgress, RacerProgress};

/// Hardcoded for now - every race uses this sentence.
pub const SENTENCE: &str = "the quick brown fox jumps over the lazy dog while the sun sets slowly behind the mountains";

const COUNTDOWN: Duration = Duration::from_secs(3);

/// Client-side state for a typing race: the local player's own progress
/// through the sentence, plus the last roster of everyone's progress
/// broadcast by the server.
pub struct RaceScreen {
    sentence: Vec<char>,
    typed: usize,
    /// `None` until `GameBegin` arrives; once set, this is also the moment
    /// input unlocks and the moment WPM is measured from.
    countdown_ends_at: Option<Instant>,
    finished_sent: bool,
    racers: Vec<RacerProgress>,
    /// True once the server says every connected racer has finished.
    all_finished: bool,
}

impl RaceScreen {
    pub fn new(sentence: String) -> Self {
        Self {
            sentence: sentence.chars().collect(),
            typed: 0,
            countdown_ends_at: None,
            finished_sent: false,
            racers: Vec::new(),
            all_finished: false,
        }
    }

    /// Called when the server broadcasts `GameBegin`: starts the local
    /// countdown, after which keystrokes are accepted.
    pub fn on_game_begin(&mut self) {
        self.countdown_ends_at = Some(Instant::now() + COUNTDOWN);
    }

    pub fn set_racers(&mut self, racers: Vec<RacerProgress>, all_finished: bool) {
        self.racers = racers;
        self.all_finished = all_finished;
    }

    fn counting_down(&self) -> bool {
        self.countdown_ends_at.is_some_and(|deadline| Instant::now() < deadline)
    }

    fn accepting_input(&self) -> bool {
        self.countdown_ends_at.is_some() && !self.counting_down()
    }

    /// Feeds one typed character in. Only the correct next character
    /// advances progress - anything else is silently ignored. Returns the
    /// progress to report to the server, if this keystroke changed it.
    pub fn handle_char(&mut self, c: char) -> Option<ClientProgress> {
        if self.finished_sent || !self.accepting_input() {
            return None;
        }
        if self.sentence.get(self.typed) != Some(&c) {
            return None;
        }

        self.typed += 1;
        let finished = self.typed == self.sentence.len();
        self.finished_sent = finished;
        let elapsed_ms = self.countdown_ends_at.unwrap().elapsed().as_millis() as u64;

        Some(ClientProgress { finished, detail: GameProgress::TypingRace { correct_chars: self.typed, elapsed_ms } })
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if self.all_finished {
            self.render_results(frame, area);
        } else {
            self.render_racing(frame, area);
        }
    }

    fn render_racing(&self, frame: &mut Frame, area: Rect) {
        let [sentence_area, gauge_area, racers_area, hint_area] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .areas(area);

        if self.counting_down() {
            let remaining = self.countdown_ends_at.unwrap().saturating_duration_since(Instant::now()).as_secs() + 1;
            let text = Paragraph::new(format!("starting in {remaining}..."));
            frame.render_widget(text, sentence_area);
        } else {
            frame.render_widget(Paragraph::new(self.sentence_line()).block(Block::bordered().title("Type this")), sentence_area);
        }

        let pct = (self.typed as f64 / self.sentence.len().max(1) as f64 * 100.0) as u16;
        frame.render_widget(Gauge::default().percent(pct).label(format!("{pct}%")), gauge_area);

        let total = self.sentence.len();
        let items: Vec<ListItem> = self.sorted_racers().into_iter().map(|r| racer_line(r, total)).collect();
        frame.render_widget(List::new(items).block(Block::bordered().title("Racers")), racers_area);

        frame.render_widget(Paragraph::new("ESC to quit"), hint_area);
    }

    fn render_results(&self, frame: &mut Frame, area: Rect) {
        let [title_area, results_area] = Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(area);

        frame.render_widget(Paragraph::new("Race over! (ESC to quit)"), title_area);

        // Already sorted by finish place, which - since everyone races the
        // same sentence from the same start signal - is equivalent to
        // ranking by WPM.
        let items: Vec<ListItem> = self
            .sorted_racers()
            .into_iter()
            .enumerate()
            .map(|(i, racer)| ListItem::new(format!("{}. {} - {:.0} wpm", i + 1, racer.username, wpm_of(racer))))
            .collect();
        frame.render_widget(List::new(items).block(Block::bordered().title("Results")), results_area);
    }

    fn sentence_line(&self) -> Line<'static> {
        let typed: String = self.sentence[..self.typed].iter().collect();
        let rest: String = self.sentence[self.typed..].iter().collect();
        Line::from(vec![
            Span::styled(typed, Style::default().fg(Color::Green)),
            Span::styled(rest, Style::default().fg(Color::DarkGray)),
        ])
    }

    /// Finished racers first (by finish place), then unfinished racers by
    /// how far they've gotten.
    fn sorted_racers(&self) -> Vec<&RacerProgress> {
        let mut racers: Vec<&RacerProgress> = self.racers.iter().collect();
        racers.sort_by_key(|r| match (r.place, &r.detail) {
            (Some(place), _) => (0, place, 0),
            (None, GameProgress::TypingRace { correct_chars, .. }) => (1, 0, u32::MAX - *correct_chars as u32),
        });
        racers
    }
}

const RACER_BAR_WIDTH: usize = 16;

/// A small, muted `[####----]`-style bar so each racer's row stays a
/// single compact line - the local player's own big `Gauge` above is
/// still the prominent one.
fn mini_bar(fraction: f64) -> String {
    let filled = (fraction.clamp(0.0, 1.0) * RACER_BAR_WIDTH as f64).round() as usize;
    format!("[{}{}]", "#".repeat(filled), "-".repeat(RACER_BAR_WIDTH - filled))
}

fn racer_line(racer: &RacerProgress, total: usize) -> ListItem<'static> {
    let GameProgress::TypingRace { correct_chars, .. } = racer.detail;
    let bar = mini_bar(correct_chars as f64 / total.max(1) as f64);
    let name = match racer.place {
        Some(place) => format!("#{place} {}", racer.username),
        None => racer.username.clone(),
    };
    let name_style = if racer.finished { Style::default().add_modifier(Modifier::BOLD) } else { Style::default() };

    ListItem::new(Line::from(vec![
        Span::styled(format!("{name:<16} "), name_style),
        Span::styled(bar, Style::default().fg(Color::DarkGray)),
    ]))
}

/// Standard WPM approximation: (characters typed / 5) per minute elapsed.
fn wpm_of(racer: &RacerProgress) -> f64 {
    let GameProgress::TypingRace { correct_chars, elapsed_ms } = racer.detail;
    let minutes = (elapsed_ms as f64 / 60_000.0).max(1.0 / 60_000.0);
    (correct_chars as f64 / 5.0) / minutes
}

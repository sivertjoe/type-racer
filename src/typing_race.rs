use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Gauge, List, ListItem, Paragraph};

use crate::protocol::{ClientProgress, GameProgress, RacerProgress};

const WORDS_PER_RACE: usize = 15;

/// A fresh random sentence for one race - see [`crate::words`].
pub fn generate_sentence() -> String {
    crate::words::random_sentence(WORDS_PER_RACE)
}

/// Countdown before a normal (multiplayer) race's `GameBegin` unlocks input.
pub const COUNTDOWN: Duration = Duration::from_secs(3);

/// Countdown for a solo race - nobody else to wait in sync with, so skip
/// straight to typing.
pub const SOLO_COUNTDOWN: Duration = Duration::from_secs(0);

/// Client-side state for a typing race: the local player's own progress
/// through the sentence, plus the last roster of everyone's progress
/// broadcast by the server.
pub struct RaceScreen {
    sentence: Vec<char>,
    typed: usize,
    /// How long the countdown lasts once `GameBegin` arrives.
    countdown: Duration,
    /// `None` until `GameBegin` arrives; once set, this is also the moment
    /// input unlocks and the moment WPM is measured from.
    countdown_ends_at: Option<Instant>,
    finished_sent: bool,
    racers: Vec<RacerProgress>,
    /// True once the server says every connected racer has finished.
    all_finished: bool,
    /// True for a knockout round this client isn't competing in - they
    /// can watch, but keystrokes do nothing.
    spectating: bool,
}

impl RaceScreen {
    pub fn new(sentence: String, countdown: Duration) -> Self {
        Self::build(sentence, countdown, false)
    }

    pub fn new_spectating(sentence: String, countdown: Duration) -> Self {
        Self::build(sentence, countdown, true)
    }

    fn build(sentence: String, countdown: Duration, spectating: bool) -> Self {
        Self {
            sentence: sentence.chars().collect(),
            typed: 0,
            countdown,
            countdown_ends_at: None,
            finished_sent: false,
            racers: Vec::new(),
            all_finished: false,
            spectating,
        }
    }

    /// Called when the server broadcasts `GameBegin`: starts the local
    /// countdown, after which keystrokes are accepted.
    pub fn on_game_begin(&mut self) {
        self.countdown_ends_at = Some(Instant::now() + self.countdown);
    }

    pub fn set_racers(&mut self, racers: Vec<RacerProgress>, all_finished: bool) {
        self.racers = racers;
        self.all_finished = all_finished;
    }

    pub fn is_over(&self) -> bool {
        self.all_finished
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
        if self.spectating || self.finished_sent || !self.accepting_input() {
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

    pub fn render(&self, frame: &mut Frame, area: Rect, is_host: bool) {
        if self.all_finished {
            self.render_results(frame, area, is_host);
        } else {
            self.render_racing(frame, area, is_host);
        }
    }

    fn render_racing(&self, frame: &mut Frame, area: Rect, is_host: bool) {
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
        } else if self.spectating {
            let text: String = self.sentence.iter().collect();
            frame.render_widget(Paragraph::new(text).block(Block::bordered().title("Watching")), sentence_area);
        } else {
            frame.render_widget(Paragraph::new(self.sentence_line()).block(Block::bordered().title("Type this")), sentence_area);
        }

        if self.spectating {
            frame.render_widget(Paragraph::new("you're spectating this round"), gauge_area);
        } else {
            let pct = (self.typed as f64 / self.sentence.len().max(1) as f64 * 100.0) as u16;
            frame.render_widget(Gauge::default().percent(pct).label(format!("{pct}%")), gauge_area);
        }

        let total = self.sentence.len();
        let items: Vec<ListItem> = self.sorted_racers().into_iter().map(|r| racer_line(r, total)).collect();
        frame.render_widget(List::new(items).block(Block::bordered().title("Racers")), racers_area);

        let hint = if is_host { "ENTER to end race early, ESC to quit" } else { "ESC to quit" };
        frame.render_widget(Paragraph::new(hint), hint_area);
    }

    fn render_results(&self, frame: &mut Frame, area: Rect, is_host: bool) {
        let [title_area, results_area] = Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(area);

        let title = if is_host { "Race over! (R to play again, ESC to quit)" } else { "Race over! (waiting for host... ESC to quit)" };
        frame.render_widget(Paragraph::new(title), title_area);

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

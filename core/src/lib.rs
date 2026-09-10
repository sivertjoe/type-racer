//! The game engine: protocol, networking, and both gamemodes' server-side
//! logic. Deliberately has no dependency on ratatui/crossterm/clap - the TUI
//! (`main.rs` and friends) is one client built on top of this, and any
//! other frontend (e.g. a web client) could be another.

pub mod knockout;
pub mod protocol;
pub mod session;
pub mod single_game;
pub mod waiting;
pub mod words;

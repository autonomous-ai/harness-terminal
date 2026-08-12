//! harness-terminal — terminal-first dive into a fleet of AI agent sessions.
//!
//! `TAB = SESSION = PANE@HOST`. A terminal where every tab is one agent session running anywhere
//! in your fleet (Claude Code, Codex, Cursor, OpenCode, PI, Hermes, Command Code, Devin, Muse,
//! AMP, Kilo, Grok — or a plain shell).

pub mod app;
pub mod config;
pub mod engines;
pub mod harness;
pub mod keys;
pub mod links;
pub mod native;
pub mod render;
pub mod restore;
pub mod session;
pub mod transport;
pub mod tui;

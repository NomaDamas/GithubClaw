//! GithubClaw TUI — SuperLightTUI-based terminal dashboard.
//!
//! Two tabs:
//! - Issue Request: Issues awaiting interactive session + embedded Claude Code session
//! - Monitoring: Active agents, rate limit status, session detail with agent timeline

pub mod app;
pub mod event;
pub mod pty;
pub mod startup;
pub mod summary;
pub mod tabs;
pub mod ui;

pub use app::App;

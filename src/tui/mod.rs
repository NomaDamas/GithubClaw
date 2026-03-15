//! GithubClaw TUI — ratatui-based terminal dashboard.
//!
//! Three tabs:
//! - Issue Request: Issues awaiting interactive session + embedded Claude Code session
//! - Monitoring: Active agents, rate limit status, session detail with agent timeline
//! - Release: Release pipeline status, checklist, dogfooding

pub mod app;
pub mod event;
pub mod tabs;
pub mod ui;

pub use app::App;

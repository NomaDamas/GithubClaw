//! Three-tier rate limit detection, hibernation, and recovery probes.
//!
//! - Tier 1 (WorkerLimited): Agent exit code indicates API rate limit -> pause new dispatches, queue events
//! - Tier 2 (OrchestratorLimited): Orchestrator session fails to respond -> full hibernate (stop feeding events)
//! - Tier 3 (FullHibernate): All API calls failing -> full hibernate + alert, server stays alive for reception
//! - Recovery: tokio timer periodically attempts lightweight API probe. On success, resume.

use std::sync::atomic::{AtomicBool, Ordering};
use tokio::time::{interval, Duration};

use crate::constants::DEFAULT_RECOVERY_PROBE_INTERVAL_SECONDS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitTier {
    /// No rate limiting active.
    None,
    /// Tier 1: pause worker dispatches, continue queuing events.
    WorkerLimited,
    /// Tier 2: stop feeding events to orchestrator.
    OrchestratorLimited,
    /// Tier 3: everything paused, server only receives webhooks.
    FullHibernate,
}

pub struct RateLimiter {
    tier: std::sync::Mutex<RateLimitTier>,
    paused: AtomicBool,
    recovery_interval: u64,
}

impl RateLimiter {
    pub fn new(recovery_interval: u64) -> Self {
        Self {
            tier: std::sync::Mutex::new(RateLimitTier::None),
            paused: AtomicBool::new(false),
            recovery_interval,
        }
    }

    /// Return the current rate limit tier.
    pub fn current_tier(&self) -> RateLimitTier {
        *self.tier.lock().unwrap()
    }

    /// Returns `true` when worker dispatches should be paused (Tier 1+).
    pub fn is_dispatch_paused(&self) -> bool {
        let tier = self.current_tier();
        matches!(
            tier,
            RateLimitTier::WorkerLimited
                | RateLimitTier::OrchestratorLimited
                | RateLimitTier::FullHibernate
        )
    }

    /// Returns `true` when orchestrator event feeding should be paused (Tier 2+).
    pub fn is_orchestrator_paused(&self) -> bool {
        let tier = self.current_tier();
        matches!(
            tier,
            RateLimitTier::OrchestratorLimited | RateLimitTier::FullHibernate
        )
    }

    /// Called when a worker agent subprocess exits with non-zero exit code.
    ///
    /// Triggered by subprocess exit code detection rather than API response parsing.
    /// Escalates to `WorkerLimited` if currently `None`, or to
    /// `FullHibernate` if already `OrchestratorLimited`.
    pub fn report_worker_rate_limit(&self) {
        let mut tier = self.tier.lock().unwrap();
        match *tier {
            RateLimitTier::None => {
                *tier = RateLimitTier::WorkerLimited;
            }
            RateLimitTier::OrchestratorLimited => {
                *tier = RateLimitTier::FullHibernate;
            }
            // Already at WorkerLimited or FullHibernate — no change needed.
            _ => {}
        }
        self.paused.store(true, Ordering::Relaxed);
    }

    /// Called when the orchestrator subprocess exits with non-zero exit code.
    ///
    /// Triggered by orchestrator subprocess exit code detection.
    /// Escalates to `OrchestratorLimited` if currently `None`, or to
    /// `FullHibernate` if already `WorkerLimited`.
    pub fn report_orchestrator_rate_limit(&self) {
        let mut tier = self.tier.lock().unwrap();
        match *tier {
            RateLimitTier::None => {
                *tier = RateLimitTier::OrchestratorLimited;
            }
            RateLimitTier::WorkerLimited => {
                *tier = RateLimitTier::FullHibernate;
            }
            // Already at OrchestratorLimited or FullHibernate — no change needed.
            _ => {}
        }
        self.paused.store(true, Ordering::Relaxed);
    }

    /// Called when the recovery probe succeeds. Resets to `None`.
    pub fn report_recovery(&self) {
        let mut tier = self.tier.lock().unwrap();
        *tier = RateLimitTier::None;
        self.paused.store(false, Ordering::Relaxed);
    }

    /// Start background recovery probe loop.
    ///
    /// Periodically attempts a lightweight `gh api rate_limit` call. On
    /// success, resets the rate limiter to `None` and resumes operations.
    pub async fn start_recovery_probe(&self) {
        let mut tick = interval(Duration::from_secs(self.recovery_interval));
        loop {
            tick.tick().await;
            if !self.paused.load(Ordering::Relaxed) {
                continue; // Not paused, skip probe
            }
            match probe_api().await {
                Ok(()) => {
                    self.report_recovery();
                    tracing::info!("Rate limit recovery: API probe succeeded, resuming");
                }
                Err(e) => {
                    tracing::warn!("Rate limit recovery probe failed: {}", e);
                }
            }
        }
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(DEFAULT_RECOVERY_PROBE_INTERVAL_SECONDS)
    }
}

/// Lightweight API probe via `gh api rate_limit`.
async fn probe_api() -> Result<(), String> {
    let output = tokio::process::Command::new("gh")
        .args(["api", "rate_limit"])
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err("API probe failed".into())
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rate_limiter_starts_at_none() {
        let rl = RateLimiter::new(300);
        assert_eq!(rl.current_tier(), RateLimitTier::None);
    }

    #[test]
    fn report_worker_rate_limit_escalates_to_worker_limited() {
        let rl = RateLimiter::new(300);
        rl.report_worker_rate_limit();
        assert_eq!(rl.current_tier(), RateLimitTier::WorkerLimited);
    }

    #[test]
    fn report_orchestrator_rate_limit_escalates_to_orchestrator_limited() {
        let rl = RateLimiter::new(300);
        rl.report_orchestrator_rate_limit();
        assert_eq!(rl.current_tier(), RateLimitTier::OrchestratorLimited);
    }

    #[test]
    fn both_reports_escalate_to_full_hibernate() {
        let rl = RateLimiter::new(300);
        rl.report_worker_rate_limit();
        rl.report_orchestrator_rate_limit();
        assert_eq!(rl.current_tier(), RateLimitTier::FullHibernate);
    }

    #[test]
    fn both_reports_reverse_order_escalate_to_full_hibernate() {
        let rl = RateLimiter::new(300);
        rl.report_orchestrator_rate_limit();
        rl.report_worker_rate_limit();
        assert_eq!(rl.current_tier(), RateLimitTier::FullHibernate);
    }

    #[test]
    fn report_recovery_resets_to_none() {
        let rl = RateLimiter::new(300);
        rl.report_worker_rate_limit();
        rl.report_orchestrator_rate_limit();
        assert_eq!(rl.current_tier(), RateLimitTier::FullHibernate);
        rl.report_recovery();
        assert_eq!(rl.current_tier(), RateLimitTier::None);
    }

    #[test]
    fn is_dispatch_paused_true_when_worker_limited_or_higher() {
        let rl = RateLimiter::new(300);
        assert!(!rl.is_dispatch_paused());

        rl.report_worker_rate_limit();
        assert!(rl.is_dispatch_paused());

        rl.report_recovery();
        rl.report_orchestrator_rate_limit();
        assert!(rl.is_dispatch_paused());

        rl.report_worker_rate_limit(); // escalate to FullHibernate
        assert!(rl.is_dispatch_paused());
    }

    #[test]
    fn is_orchestrator_paused_true_when_orchestrator_limited_or_higher() {
        let rl = RateLimiter::new(300);
        assert!(!rl.is_orchestrator_paused());

        // WorkerLimited alone should NOT pause orchestrator
        rl.report_worker_rate_limit();
        assert!(!rl.is_orchestrator_paused());

        rl.report_recovery();

        // OrchestratorLimited should pause orchestrator
        rl.report_orchestrator_rate_limit();
        assert!(rl.is_orchestrator_paused());

        // FullHibernate should also pause orchestrator
        rl.report_worker_rate_limit();
        assert_eq!(rl.current_tier(), RateLimitTier::FullHibernate);
        assert!(rl.is_orchestrator_paused());
    }

    #[test]
    fn paused_flag_tracks_tier_state() {
        let rl = RateLimiter::new(300);
        assert!(!rl.paused.load(Ordering::Relaxed));

        rl.report_worker_rate_limit();
        assert!(rl.paused.load(Ordering::Relaxed));

        rl.report_recovery();
        assert!(!rl.paused.load(Ordering::Relaxed));
    }
}

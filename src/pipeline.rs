//! V2 Pipeline state machine for Issue Request lifecycle.
//!
//! Manages the full lifecycle: classification → approval → implementation → merge.
//! Each issue progresses through discrete states, driven by GitHub HTML markers.

use serde::{Deserialize, Serialize};

use crate::constants::{
    BUG_REPRODUCER_MAX_INFO_REQUESTS, IMPLEMENTER_REVIEWER_MAX_LOOP,
    IMPLEMENTER_VERIFIER_MAX_LOOP,
};
use crate::markers::MarkerType;

// ---------------------------------------------------------------------------
// Issue classification
// ---------------------------------------------------------------------------

/// Classification of an incoming issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueType {
    Bug,
    Feature,
    Refactoring,
}

// ---------------------------------------------------------------------------
// Pipeline state
// ---------------------------------------------------------------------------

/// Current state of an issue in the pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineState {
    /// Orchestrator is classifying the issue.
    Classifying,
    /// Bug Reproducer is attempting reproduction.
    Reproducing,
    /// Waiting for reporter to provide additional info (bug reproduction failed).
    AwaitingInfo { attempts: u32 },
    /// Vision-gap Analyst is running (Feature/Refactoring).
    AnalyzingVision,
    /// Waiting for human Interactive Session (Feature/Refactoring).
    AwaitingInteractiveSession,
    /// Issue approved — implementation pipeline starting.
    Approved,
    /// Orchestrator is decomposing into sub-issues.
    Decomposing,
    /// Verifier is writing test code.
    WritingTests { sub_issue: u64 },
    /// Implementer is coding.
    Implementing { sub_issue: u64, attempt: u32 },
    /// Reviewer is reviewing code.
    Reviewing { sub_issue: u64, attempt: u32 },
    /// Verifier is performing e2e validation.
    VerifyingE2e,
    /// Human escalation needed (loop limit exceeded).
    Stuck { reason: String },
    /// Feature branch merged to dev.
    Merged,
    /// Issue rejected/closed.
    Rejected { reason: String },
}

// ---------------------------------------------------------------------------
// Pipeline tracker
// ---------------------------------------------------------------------------

/// Tracks the pipeline state for a single root issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineTracker {
    pub issue_id: u64,
    pub issue_type: Option<IssueType>,
    pub state: PipelineState,
    pub sub_issues: Vec<u64>,
    pub current_sub_issue_index: usize,
    pub branch_name: Option<String>,
}

impl PipelineTracker {
    /// Create a new tracker for an incoming issue.
    pub fn new(issue_id: u64) -> Self {
        Self {
            issue_id,
            issue_type: None,
            state: PipelineState::Classifying,
            sub_issues: Vec::new(),
            current_sub_issue_index: 0,
            branch_name: None,
        }
    }

    /// Process a marker event and return the next action to take.
    pub fn on_marker(&mut self, marker: &MarkerType, attrs: &std::collections::HashMap<String, String>) -> PipelineAction {
        match (&self.state, marker) {
            // Bug: reproduced=true → auto-approve
            (PipelineState::Reproducing, MarkerType::Reproduced) => {
                let reproduced = attrs.get("reproduced").map(|v| v == "true").unwrap_or(false);
                if reproduced {
                    self.state = PipelineState::Approved;
                    PipelineAction::PostMarker(MarkerType::Approved)
                } else {
                    let attempts = match &self.state {
                        PipelineState::AwaitingInfo { attempts } => *attempts,
                        _ => 0,
                    };
                    let new_attempts = attempts + 1;
                    if new_attempts >= BUG_REPRODUCER_MAX_INFO_REQUESTS {
                        self.state = PipelineState::Rejected {
                            reason: "Bug could not be reproduced after maximum attempts".into(),
                        };
                        PipelineAction::CloseIssue("Could not reproduce after 3 attempts.".into())
                    } else {
                        self.state = PipelineState::AwaitingInfo { attempts: new_attempts };
                        PipelineAction::RequestInfo
                    }
                }
            }

            // Approved → start implementation (decompose first)
            (PipelineState::Approved, MarkerType::Approved) |
            (PipelineState::AwaitingInteractiveSession, MarkerType::Approved) => {
                self.state = PipelineState::Decomposing;
                PipelineAction::Decompose
            }

            // After decomposition, start first sub-issue: write tests
            (PipelineState::Decomposing, _) => {
                self.advance_to_next_sub_issue()
            }

            // Reviewed → next sub-issue or e2e
            (PipelineState::Reviewing { .. }, MarkerType::Reviewed) => {
                self.current_sub_issue_index += 1;
                self.advance_to_next_sub_issue()
            }

            // Verified → merge to dev
            (PipelineState::VerifyingE2e, MarkerType::Verified) => {
                self.state = PipelineState::Merged;
                PipelineAction::MergeToDev
            }

            // Stuck → wait for human
            (_, MarkerType::Stuck) => {
                self.state = PipelineState::Stuck {
                    reason: "Loop limit exceeded".into(),
                };
                PipelineAction::WaitForHuman
            }

            _ => PipelineAction::None,
        }
    }

    /// Handle implementer completion (tests pass/fail).
    pub fn on_implementer_done(&mut self, tests_passed: bool) -> PipelineAction {
        match &self.state {
            PipelineState::Implementing { sub_issue, attempt } => {
                let sub = *sub_issue;
                let att = *attempt;
                if tests_passed {
                    // Move to review
                    self.state = PipelineState::Reviewing { sub_issue: sub, attempt: 1 };
                    PipelineAction::DispatchReviewer(sub)
                } else if att >= IMPLEMENTER_VERIFIER_MAX_LOOP {
                    self.state = PipelineState::Stuck {
                        reason: format!("Implementer failed to pass tests after {} attempts", att),
                    };
                    PipelineAction::PostStuck(format!(
                        "Implementer-Verifier loop exceeded {} iterations for sub-issue #{}",
                        IMPLEMENTER_VERIFIER_MAX_LOOP, sub
                    ))
                } else {
                    self.state = PipelineState::Implementing { sub_issue: sub, attempt: att + 1 };
                    PipelineAction::DispatchImplementer(sub)
                }
            }
            _ => PipelineAction::None,
        }
    }

    /// Handle reviewer feedback (pass/fail).
    pub fn on_review_done(&mut self, passed: bool) -> PipelineAction {
        match &self.state {
            PipelineState::Reviewing { sub_issue, attempt } => {
                let sub = *sub_issue;
                let att = *attempt;
                if passed {
                    // Advance to next sub-issue or e2e
                    self.current_sub_issue_index += 1;
                    self.advance_to_next_sub_issue()
                } else if att >= IMPLEMENTER_REVIEWER_MAX_LOOP {
                    self.state = PipelineState::Stuck {
                        reason: format!("Review loop exceeded {} rounds for sub-issue #{}", att, sub),
                    };
                    PipelineAction::PostStuck(format!(
                        "Reviewer-Implementer loop exceeded {} iterations for sub-issue #{}",
                        IMPLEMENTER_REVIEWER_MAX_LOOP, sub
                    ))
                } else {
                    // Back to implementer for fixes
                    self.state = PipelineState::Implementing { sub_issue: sub, attempt: att + 1 };
                    PipelineAction::DispatchImplementer(sub)
                }
            }
            _ => PipelineAction::None,
        }
    }

    /// Classify the issue and transition to the appropriate state.
    pub fn classify(&mut self, issue_type: IssueType) -> PipelineAction {
        self.issue_type = Some(issue_type.clone());
        match issue_type {
            IssueType::Bug => {
                self.state = PipelineState::Reproducing;
                PipelineAction::DispatchBugReproducer
            }
            IssueType::Feature | IssueType::Refactoring => {
                self.state = PipelineState::AnalyzingVision;
                PipelineAction::DispatchVisionGapAnalyst
            }
        }
    }

    /// Vision-gap analysis complete → await interactive session.
    pub fn on_vision_analysis_done(&mut self) -> PipelineAction {
        self.state = PipelineState::AwaitingInteractiveSession;
        PipelineAction::AwaitInteractiveSession
    }

    /// Set sub-issues after decomposition.
    pub fn set_sub_issues(&mut self, sub_issues: Vec<u64>) {
        self.sub_issues = sub_issues;
        self.current_sub_issue_index = 0;
    }

    /// Handle human direction after stuck state.
    pub fn on_human_direction(&mut self) -> PipelineAction {
        // Reset to implementing the current sub-issue with counter reset
        if let Some(&sub) = self.sub_issues.get(self.current_sub_issue_index) {
            self.state = PipelineState::Implementing { sub_issue: sub, attempt: 1 };
            PipelineAction::DispatchImplementer(sub)
        } else {
            PipelineAction::None
        }
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn advance_to_next_sub_issue(&mut self) -> PipelineAction {
        if self.current_sub_issue_index >= self.sub_issues.len() {
            // All sub-issues done → e2e verification
            self.state = PipelineState::VerifyingE2e;
            PipelineAction::DispatchVerifierE2e
        } else {
            let sub = self.sub_issues[self.current_sub_issue_index];
            self.state = PipelineState::WritingTests { sub_issue: sub };
            PipelineAction::DispatchVerifierTests(sub)
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline actions (output of state transitions)
// ---------------------------------------------------------------------------

/// Actions the pipeline engine requests after a state transition.
#[derive(Debug, Clone, PartialEq)]
pub enum PipelineAction {
    /// No action needed.
    None,
    /// Post an approval marker on the issue.
    PostMarker(MarkerType),
    /// Close the issue with a reason.
    CloseIssue(String),
    /// Request additional info from the issue reporter.
    RequestInfo,
    /// Decompose the issue into sub-issues.
    Decompose,
    /// Dispatch Bug Reproducer agent.
    DispatchBugReproducer,
    /// Dispatch Vision-gap Analyst agent.
    DispatchVisionGapAnalyst,
    /// Wait for human Interactive Session.
    AwaitInteractiveSession,
    /// Dispatch Verifier to write tests for a sub-issue.
    DispatchVerifierTests(u64),
    /// Dispatch Implementer for a sub-issue.
    DispatchImplementer(u64),
    /// Dispatch Reviewer for a sub-issue.
    DispatchReviewer(u64),
    /// Dispatch Verifier for e2e validation.
    DispatchVerifierE2e,
    /// Post stuck marker with reason.
    PostStuck(String),
    /// Wait for human direction.
    WaitForHuman,
    /// Merge feature branch to dev.
    MergeToDev,
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // 1. New tracker starts in Classifying
    #[test]
    fn new_tracker_starts_classifying() {
        let t = PipelineTracker::new(42);
        assert_eq!(t.state, PipelineState::Classifying);
        assert_eq!(t.issue_id, 42);
        assert!(t.issue_type.is_none());
    }

    // 2. Classify as Bug → Reproducing
    #[test]
    fn classify_bug_dispatches_reproducer() {
        let mut t = PipelineTracker::new(1);
        let action = t.classify(IssueType::Bug);
        assert_eq!(t.state, PipelineState::Reproducing);
        assert_eq!(action, PipelineAction::DispatchBugReproducer);
    }

    // 3. Classify as Feature → AnalyzingVision
    #[test]
    fn classify_feature_dispatches_vision_analyst() {
        let mut t = PipelineTracker::new(1);
        let action = t.classify(IssueType::Feature);
        assert_eq!(t.state, PipelineState::AnalyzingVision);
        assert_eq!(action, PipelineAction::DispatchVisionGapAnalyst);
    }

    // 4. Bug reproduced=true → Approved
    #[test]
    fn bug_reproduced_true_auto_approves() {
        let mut t = PipelineTracker::new(1);
        t.classify(IssueType::Bug);

        let mut attrs = HashMap::new();
        attrs.insert("reproduced".into(), "true".into());
        let action = t.on_marker(&MarkerType::Reproduced, &attrs);

        assert_eq!(t.state, PipelineState::Approved);
        assert_eq!(action, PipelineAction::PostMarker(MarkerType::Approved));
    }

    // 5. Bug reproduced=false → AwaitingInfo
    #[test]
    fn bug_not_reproduced_requests_info() {
        let mut t = PipelineTracker::new(1);
        t.classify(IssueType::Bug);

        let mut attrs = HashMap::new();
        attrs.insert("reproduced".into(), "false".into());
        let action = t.on_marker(&MarkerType::Reproduced, &attrs);

        assert!(matches!(t.state, PipelineState::AwaitingInfo { attempts: 1 }));
        assert_eq!(action, PipelineAction::RequestInfo);
    }

    // 6. Vision analysis done → AwaitingInteractiveSession
    #[test]
    fn vision_done_awaits_session() {
        let mut t = PipelineTracker::new(1);
        t.classify(IssueType::Feature);
        let action = t.on_vision_analysis_done();
        assert_eq!(t.state, PipelineState::AwaitingInteractiveSession);
        assert_eq!(action, PipelineAction::AwaitInteractiveSession);
    }

    // 7. Approved → Decomposing
    #[test]
    fn approved_triggers_decompose() {
        let mut t = PipelineTracker::new(1);
        t.state = PipelineState::AwaitingInteractiveSession;
        let action = t.on_marker(&MarkerType::Approved, &HashMap::new());
        assert_eq!(t.state, PipelineState::Decomposing);
        assert_eq!(action, PipelineAction::Decompose);
    }

    // 8. Sub-issue flow: tests → implement → review
    #[test]
    fn sub_issue_flow_tests_implement_review() {
        let mut t = PipelineTracker::new(1);
        t.state = PipelineState::Approved;
        t.set_sub_issues(vec![100, 101]);

        // Advance from decomposing to first sub-issue
        let action = t.advance_to_next_sub_issue();
        assert_eq!(action, PipelineAction::DispatchVerifierTests(100));
        assert!(matches!(t.state, PipelineState::WritingTests { sub_issue: 100 }));

        // Tests written, now implementing
        t.state = PipelineState::Implementing { sub_issue: 100, attempt: 1 };
        let action = t.on_implementer_done(true);
        assert_eq!(action, PipelineAction::DispatchReviewer(100));
        assert!(matches!(t.state, PipelineState::Reviewing { sub_issue: 100, .. }));

        // Review passed → next sub-issue
        let action = t.on_review_done(true);
        assert_eq!(action, PipelineAction::DispatchVerifierTests(101));
    }

    // 9. All sub-issues done → e2e
    #[test]
    fn all_sub_issues_done_triggers_e2e() {
        let mut t = PipelineTracker::new(1);
        t.set_sub_issues(vec![100]);
        t.current_sub_issue_index = 0;
        t.state = PipelineState::Reviewing { sub_issue: 100, attempt: 1 };

        let action = t.on_review_done(true);
        assert_eq!(t.state, PipelineState::VerifyingE2e);
        assert_eq!(action, PipelineAction::DispatchVerifierE2e);
    }

    // 10. Verified → MergeToDev
    #[test]
    fn verified_merges_to_dev() {
        let mut t = PipelineTracker::new(1);
        t.state = PipelineState::VerifyingE2e;
        let action = t.on_marker(&MarkerType::Verified, &HashMap::new());
        assert_eq!(t.state, PipelineState::Merged);
        assert_eq!(action, PipelineAction::MergeToDev);
    }

    // 11. Implementer exceeds loop limit → Stuck
    #[test]
    fn implementer_exceeds_loop_limit_stuck() {
        let mut t = PipelineTracker::new(1);
        t.state = PipelineState::Implementing {
            sub_issue: 100,
            attempt: IMPLEMENTER_VERIFIER_MAX_LOOP,
        };
        let action = t.on_implementer_done(false);
        assert!(matches!(t.state, PipelineState::Stuck { .. }));
        assert!(matches!(action, PipelineAction::PostStuck(_)));
    }

    // 12. Reviewer exceeds loop limit → Stuck
    #[test]
    fn reviewer_exceeds_loop_limit_stuck() {
        let mut t = PipelineTracker::new(1);
        t.state = PipelineState::Reviewing {
            sub_issue: 100,
            attempt: IMPLEMENTER_REVIEWER_MAX_LOOP,
        };
        let action = t.on_review_done(false);
        assert!(matches!(t.state, PipelineState::Stuck { .. }));
        assert!(matches!(action, PipelineAction::PostStuck(_)));
    }

    // 13. Stuck marker transitions to Stuck state
    #[test]
    fn stuck_marker_transitions() {
        let mut t = PipelineTracker::new(1);
        t.state = PipelineState::Implementing { sub_issue: 100, attempt: 5 };
        let action = t.on_marker(&MarkerType::Stuck, &HashMap::new());
        assert!(matches!(t.state, PipelineState::Stuck { .. }));
        assert_eq!(action, PipelineAction::WaitForHuman);
    }

    // 14. Human direction resets loop
    #[test]
    fn human_direction_resets_loop() {
        let mut t = PipelineTracker::new(1);
        t.set_sub_issues(vec![100, 101]);
        t.current_sub_issue_index = 0;
        t.state = PipelineState::Stuck { reason: "test".into() };

        let action = t.on_human_direction();
        assert!(matches!(t.state, PipelineState::Implementing { sub_issue: 100, attempt: 1 }));
        assert_eq!(action, PipelineAction::DispatchImplementer(100));
    }

    // 15. Classify as Refactoring → same as Feature
    #[test]
    fn classify_refactoring_same_as_feature() {
        let mut t = PipelineTracker::new(1);
        let action = t.classify(IssueType::Refactoring);
        assert_eq!(t.state, PipelineState::AnalyzingVision);
        assert_eq!(action, PipelineAction::DispatchVisionGapAnalyst);
        assert_eq!(t.issue_type, Some(IssueType::Refactoring));
    }

    // 16. No sub-issues → direct to e2e
    #[test]
    fn no_sub_issues_goes_to_e2e() {
        let mut t = PipelineTracker::new(1);
        t.set_sub_issues(vec![]);
        let action = t.advance_to_next_sub_issue();
        assert_eq!(t.state, PipelineState::VerifyingE2e);
        assert_eq!(action, PipelineAction::DispatchVerifierE2e);
    }

    // 17. Serialization roundtrip
    #[test]
    fn serialization_roundtrip() {
        let mut t = PipelineTracker::new(42);
        t.classify(IssueType::Bug);
        t.set_sub_issues(vec![100, 101]);
        t.branch_name = Some("Feature/#42".into());

        let json = serde_json::to_string(&t).unwrap();
        let deserialized: PipelineTracker = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.issue_id, 42);
        assert_eq!(deserialized.issue_type, Some(IssueType::Bug));
        assert_eq!(deserialized.sub_issues, vec![100, 101]);
    }

    // 18. Review fail loops back to implementer
    #[test]
    fn review_fail_loops_to_implementer() {
        let mut t = PipelineTracker::new(1);
        t.set_sub_issues(vec![100]);
        t.state = PipelineState::Reviewing { sub_issue: 100, attempt: 3 };

        let action = t.on_review_done(false);
        assert!(matches!(t.state, PipelineState::Implementing { sub_issue: 100, attempt: 4 }));
        assert_eq!(action, PipelineAction::DispatchImplementer(100));
    }
}

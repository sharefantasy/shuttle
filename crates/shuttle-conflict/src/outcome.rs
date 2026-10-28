//! Lowering of `shuttle-conflict`'s native receipt into the universal
//! [`shuttle_protocol::ToolOutcome`] envelope (efficiency bet #2, P1.2).
//!
//! `shuttle-conflict` is above `shuttle-protocol`, so the mapping lives here. A resolve
//! run emits a [`ConflictReceipt`] (one [`HunkReceipt`] per hunk). This module
//! folds it into a single [`ToolOutcome`] a router can read without walking the
//! per-hunk list.
//!
//! # The mapping (§7 never-guess)
//! * **All hunks verified** → [`Green`] (`ok`). The merge is done.
//! * **Some hunks not verified** → [`Blocked`] (`ambiguous`). Not an error: the
//!   one concrete next action is to decide those hunks (a human/model picks a
//!   side). The unresolved hunk ids are listed in `whats_missing`. The tool
//!   never auto-picks a side to look Green.
//!
//! [`Green`]: shuttle_protocol::OutcomeState::Green
//! [`Blocked`]: shuttle_protocol::OutcomeState::Blocked

use crate::ConflictReceipt;
use shuttle_protocol::{reason, ConflictDetail, ToolOutcome, TypedDetail};

/// Tool name used in every conflict-produced envelope.
pub const TOOL_CONFLICT: &str = "conflict";

/// Lower a [`ConflictReceipt`] into a [`ToolOutcome`].
///
/// `path` is the conflicted file the receipt is about (the receipt itself keeps
/// per-hunk `file`s, but the envelope carries the run's primary path).
pub fn conflict_outcome(path: &str, receipt: &ConflictReceipt) -> ToolOutcome {
    let total = receipt.hunks.len();
    let unresolved_ids: Vec<usize> = receipt
        .hunks
        .iter()
        .filter(|h| !h.verify_passed)
        .map(|h| h.hunk_id)
        .collect();
    let unresolved = unresolved_ids.len();
    let auto_resolved = total - unresolved;

    let detail = TypedDetail::Conflict(ConflictDetail {
        path: path.to_string(),
        hunks: total,
        auto_resolved,
        unresolved,
    });

    if receipt.all_verified && unresolved == 0 {
        ToolOutcome::green(TOOL_CONFLICT, detail)
            .with_summary(format!("{path}: {total} hunk(s) resolved and verified"))
    } else {
        let missing: Vec<String> = unresolved_ids
            .iter()
            .map(|id| format!("hunk #{id} not verified"))
            .collect();
        ToolOutcome::blocked(
            TOOL_CONFLICT,
            reason::AMBIGUOUS,
            format!(
                "{unresolved} of {total} hunk(s) in `{path}` need a decision; pick a side per unresolved hunk"
            ),
            detail,
        )
        .with_missing(missing)
        .with_summary(format!("{path}: {unresolved}/{total} hunk(s) unresolved"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConflictLevel, HunkReceipt};
    use shuttle_protocol::OutcomeState;

    fn receipt(all_verified: bool, hunks: Vec<HunkReceipt>) -> ConflictReceipt {
        ConflictReceipt {
            run_id: "r1".into(),
            hunks,
            all_verified,
        }
    }

    fn hunk(id: usize, verify_passed: bool) -> HunkReceipt {
        HunkReceipt {
            hunk_id: id,
            file: "a.rs".into(),
            level: ConflictLevel::Structural,
            strategy: "auto".into(),
            model_involved: false,
            verify_passed,
        }
    }

    #[test]
    fn all_verified_is_green() {
        let r = receipt(true, vec![hunk(0, true), hunk(1, true)]);
        let o = conflict_outcome("a.rs", &r);
        assert_eq!(o.state, OutcomeState::Green);
        assert_eq!(o.reason_code, reason::OK);
        match o.detail {
            TypedDetail::Conflict(c) => {
                assert_eq!(c.hunks, 2);
                assert_eq!(c.auto_resolved, 2);
                assert_eq!(c.unresolved, 0);
            }
            _ => panic!("wrong detail"),
        }
    }

    #[test]
    fn unresolved_hunks_block_ambiguous() {
        let r = receipt(false, vec![hunk(0, true), hunk(1, false)]);
        let o = conflict_outcome("a.rs", &r);
        assert_eq!(o.state, OutcomeState::Blocked);
        assert_eq!(o.reason_code, reason::AMBIGUOUS);
        assert!(!o.state.is_error());
        assert_eq!(o.whats_missing, vec!["hunk #1 not verified".to_string()]);
    }
}

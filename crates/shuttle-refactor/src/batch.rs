//! Transactional batch refactor (docs §8: all-or-nothing composition).
//!
//! A single refactor already re-parses its own file (see [`crate::verify`]).
//! But a *sequence* of refactors — e.g. rename-symbol then remove-parameter —
//! must compose atomically: if step 3 of 5 fails to anchor, the two edits that
//! already succeeded must not reach disk. Otherwise the tree is left in a
//! half-refactored state that neither the old nor the new intent describes.
//!
//! This module is the pure, disk-free core. It threads every plan's ops through
//! an in-memory virtual filesystem ([`Vfs`]) in order, so a later step *sees*
//! the edits of the earlier ones (a rename feeds the next signature change).
//! The whole batch is judged before any commit:
//!
//!   1. Every step's plan must clear the confidence gate (Low is refused unless
//!      `allow_low`, mirroring the CLI's §9.5 `--force`).
//!   2. Every op must apply cleanly (a single [`shuttle_editcore`] anchor miss,
//!      ambiguity, or a bad Insert/CreateFile aborts the batch).
//!   3. After all steps, every file the batch touched is re-parsed; if the
//!      composed edits introduced a *new* syntax error the batch is rejected.
//!
//! On any failure `apply_batch` returns `Err` and the caller's `initial` VFS is
//! left untouched (we mutate a clone). Only on full success does it return the
//! set of changed files for the caller to write — the CLI wraps that write in a
//! snapshot/rollback so even a mid-write IO error is undone.

use std::collections::BTreeMap;

use shuttle_nav::Confidence;
use shuttle_protocol::{ApplyStatus, EditOp, InsertPosition};

use crate::locals::Lang;
use crate::verify::error_count;
use crate::RefactorPlan;

/// An in-memory file map: `path -> current content`. A path that is absent is
/// treated as a non-existent file: `CreateFile` may create it, but a
/// `SearchReplace`/`Insert` against it is a hard error.
pub type Vfs = BTreeMap<String, String>;

/// One planned refactor in a batch: a human label plus its plan.
#[derive(Debug, Clone)]
pub struct BatchStep {
    pub label: String,
    pub plan: RefactorPlan,
}

impl BatchStep {
    pub fn new(label: impl Into<String>, plan: RefactorPlan) -> Self {
        Self {
            label: label.into(),
            plan,
        }
    }
}

/// Why a batch was rejected. Every variant names the offending step so the CLI
/// can report exactly which refactor broke the chain.
#[derive(Debug, Clone)]
pub enum BatchError {
    /// A step's plan was Low-confidence and `allow_low` was not set.
    LowConfidence {
        step: usize,
        label: String,
        warnings: Vec<String>,
    },
    /// An op in `step` did not apply cleanly against the (composed) VFS.
    OpFailed {
        step: usize,
        label: String,
        path: String,
        status: ApplyStatus,
    },
    /// The composed edits introduced new syntax errors in `path`.
    Reparse {
        step_count: usize,
        path: String,
        before: usize,
        after: usize,
    },
}

impl std::fmt::Display for BatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BatchError::LowConfidence { step, label, .. } => write!(
                f,
                "step {} ({label}) is Low-confidence — batch refused (use --force to override)",
                step + 1
            ),
            BatchError::OpFailed { step, label, path, status } => write!(
                f,
                "step {} ({label}) failed to apply to {path}: {status:?} — batch aborted (all-or-nothing)",
                step + 1
            ),
            BatchError::Reparse { path, before, after, .. } => write!(
                f,
                "composed batch left {path} with {} new syntax-error node(s) ({before} → {after}) — batch refused",
                after - before
            ),
        }
    }
}

impl std::error::Error for BatchError {}

/// The successful result of a batch: only the files whose content changed,
/// mapped to their final content, plus a per-step audit line.
#[derive(Debug, Clone)]
pub struct BatchOutcome {
    pub changed: Vfs,
    pub steps: Vec<String>,
}

/// Apply `steps` in order against `initial`, all-or-nothing. Later steps observe
/// the edits of earlier ones. Returns the changed files on full success, or the
/// first blocking [`BatchError`] (leaving the caller's data untouched — we work
/// on a clone).
pub fn apply_batch(
    initial: &Vfs,
    steps: &[BatchStep],
    allow_low: bool,
) -> Result<BatchOutcome, BatchError> {
    let mut vfs = initial.clone();
    let mut audit: Vec<String> = Vec::with_capacity(steps.len());

    for (i, step) in steps.iter().enumerate() {
        // 1. Confidence gate (mirrors the single-refactor §9.5 gate).
        if matches!(step.plan.confidence, Confidence::Low) && !allow_low {
            return Err(BatchError::LowConfidence {
                step: i,
                label: step.label.clone(),
                warnings: step.plan.warnings.clone(),
            });
        }

        // 2. Apply every op against the composed VFS.
        let mut applied = 0usize;
        for op in &step.plan.plan.ops {
            apply_op(&mut vfs, op).map_err(|(path, status)| BatchError::OpFailed {
                step: i,
                label: step.label.clone(),
                path,
                status,
            })?;
            applied += 1;
        }
        audit.push(format!(
            "step {}: {} — {applied} op(s) applied",
            i + 1,
            step.label
        ));
    }

    // 3. Re-parse every changed file once, comparing against its pre-batch text.
    for (path, new_content) in &vfs {
        let Some(lang) = Lang::from_path(path) else {
            continue;
        };
        let before = match initial.get(path) {
            Some(orig) => error_count(lang, orig),
            None => 0, // freshly created file: judge on its own
        };
        let after = error_count(lang, new_content);
        if after > before {
            return Err(BatchError::Reparse {
                step_count: steps.len(),
                path: path.clone(),
                before,
                after,
            });
        }
    }

    // Emit only the files whose content actually changed (or are new).
    let mut changed: Vfs = BTreeMap::new();
    for (path, content) in vfs {
        match initial.get(&path) {
            Some(orig) if *orig == content => {}
            _ => {
                changed.insert(path, content);
            }
        }
    }

    Ok(BatchOutcome {
        changed,
        steps: audit,
    })
}

/// Thread a step's ops through `vfs` in place — no confidence gate, no reparse.
/// This is the primitive the CLI uses to derive the *intermediate* source text
/// that each subsequent step must be planned against (a later refactor must see
/// the edits of the earlier ones). The atomic gate + composed reparse still run
/// in [`apply_batch`] over the full step list before anything reaches disk.
pub fn compose_ops(vfs: &mut Vfs, ops: &[EditOp]) -> Result<(), (String, ApplyStatus)> {
    for op in ops {
        apply_op(vfs, op)?;
    }
    Ok(())
}

/// Apply one op against the VFS in place. Returns `Err((path, status))` when the
/// op cannot be applied cleanly.
fn apply_op(vfs: &mut Vfs, op: &EditOp) -> Result<(), (String, ApplyStatus)> {
    match op {
        EditOp::SearchReplace {
            path,
            search,
            replace,
            line_hint,
        } => {
            let Some(source) = vfs.get(path) else {
                return Err((path.clone(), ApplyStatus::AnchorMiss));
            };
            let (next, outcome) =
                shuttle_editcore::apply_search_replace(source, search, replace, *line_hint);
            if matches!(outcome.status, ApplyStatus::Applied) {
                vfs.insert(path.clone(), next);
                Ok(())
            } else {
                Err((path.clone(), outcome.status))
            }
        }
        EditOp::CreateFile { path, content } => {
            vfs.insert(path.clone(), content.clone());
            Ok(())
        }
        EditOp::Insert {
            path,
            anchor,
            position,
            content,
        } => {
            let Some(source) = vfs.get(path) else {
                return Err((path.clone(), ApplyStatus::AnchorMiss));
            };
            match insert_at(source, anchor, *position, content) {
                Ok(next) => {
                    vfs.insert(path.clone(), next);
                    Ok(())
                }
                Err(status) => Err((path.clone(), status)),
            }
        }
        // Codemod ops are opaque to the tree-sitter core (they defer to
        // ast-grep at apply time); a batch cannot compose them in memory.
        EditOp::Codemod { .. } => Err((String::new(), ApplyStatus::Error)),
    }
}

/// Line-based anchored insert, mirroring the CLI's `apply_insert` so the batch
/// core previews the exact same result the writer will produce.
fn insert_at(
    source: &str,
    anchor: &str,
    position: InsertPosition,
    content: &str,
) -> Result<String, ApplyStatus> {
    let lines: Vec<&str> = source.lines().collect();
    let hits: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.contains(anchor))
        .map(|(i, _)| i)
        .collect();
    match hits.len() {
        0 => Err(ApplyStatus::AnchorMiss),
        1 => {
            let idx = hits[0];
            let at = match position {
                InsertPosition::Before => idx,
                InsertPosition::After => idx + 1,
            };
            let mut out: Vec<&str> = Vec::with_capacity(lines.len() + 1);
            out.extend_from_slice(&lines[..at]);
            for cl in content.lines() {
                out.push(cl);
            }
            out.extend_from_slice(&lines[at..]);
            let mut joined = out.join("\n");
            if source.ends_with('\n') {
                joined.push('\n');
            }
            Ok(joined)
        }
        _ => Err(ApplyStatus::Ambiguous),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{change_signature, remove_parameter, NewParam};

    fn vfs(pairs: &[(&str, &str)]) -> Vfs {
        pairs
            .iter()
            .map(|(p, c)| (p.to_string(), c.to_string()))
            .collect()
    }

    #[test]
    fn composes_two_same_file_refactors_atomically() {
        // Step 1 drops the unused trailing parameter; step 2 reorders the two
        // remaining value parameters. Step 2 must plan against the *post-step-1*
        // text, so we thread the intermediate VFS through.
        let src = "def f(a, b, unused):\n    return a - b\n\nf(1, 2, 9)\nf(3, 4, 8)\n";
        let files = vfs(&[("m.py", src)]);

        let p1 = remove_parameter("m.py", src, "f", "unused", "").unwrap();
        let mid = apply_batch(&files, &[BatchStep::new("drop unused", p1.clone())], false)
            .unwrap()
            .changed;
        let mid_src = mid.get("m.py").unwrap();
        // reorder (a, b) -> (b, a)
        let reorder = vec![NewParam::keep(1), NewParam::keep(0)];
        let p2 = change_signature("m.py", mid_src, "f", &reorder, "").unwrap();

        let out = apply_batch(
            &files,
            &[
                BatchStep::new("drop unused", p1),
                BatchStep::new("reorder a,b", p2),
            ],
            false,
        )
        .unwrap();

        let final_src = out.changed.get("m.py").unwrap();
        assert!(final_src.contains("def f(b, a):"), "got: {final_src}");
        assert!(final_src.contains("f(2, 1)"), "got: {final_src}");
        assert!(final_src.contains("f(4, 3)"), "got: {final_src}");
        assert!(
            !final_src.contains("unused"),
            "unused should be gone: {final_src}"
        );
        assert_eq!(out.steps.len(), 2);
    }

    #[test]
    fn a_failing_step_aborts_the_whole_batch() {
        let src = "def f(a, unused):\n    return a\n\nf(1, 2)\n";
        let files = vfs(&[("m.py", src)]);
        // Step 1 is a real refactor; step 2 is a hand-crafted plan whose op
        // anchors on text that step 1 already removed, so it must fail + abort.
        let good = remove_parameter("m.py", src, "f", "unused", "").unwrap();
        let mut bogus = remove_parameter("m.py", src, "f", "unused", "").unwrap();
        bogus.plan.ops = vec![crate::search_replace(
            "m.py",
            "def f(a, unused):".into(), // after step 1 the line is `def f(a):`
            "def f():".into(),
            1,
        )];

        let err = apply_batch(
            &files,
            &[
                BatchStep::new("drop unused", good),
                BatchStep::new("bogus", bogus),
            ],
            false,
        )
        .unwrap_err();
        match err {
            BatchError::OpFailed { step, path, .. } => {
                assert_eq!(step, 1);
                assert_eq!(path, "m.py");
            }
            other => panic!("expected OpFailed, got {other:?}"),
        }
    }

    #[test]
    fn low_confidence_step_is_refused_without_allow_low() {
        let src = "def f(a, unused):\n    return a\n\nf(1, 2)\n";
        let files = vfs(&[("m.py", src)]);
        let mut low = remove_parameter("m.py", src, "f", "unused", "").unwrap();
        low.confidence = Confidence::Low;

        let err = apply_batch(&files, &[BatchStep::new("low", low.clone())], false).unwrap_err();
        assert!(matches!(err, BatchError::LowConfidence { step: 0, .. }));

        // With allow_low it goes through.
        let ok = apply_batch(&files, &[BatchStep::new("low", low)], true).unwrap();
        assert!(ok.changed.contains_key("m.py"));
    }

    #[test]
    fn unchanged_files_are_not_emitted() {
        let src = "def f(a, unused):\n    return a\n\nf(1, 2)\n";
        let files = vfs(&[("m.py", src), ("other.py", "x = 1\n")]);
        let p = remove_parameter("m.py", src, "f", "unused", "").unwrap();
        let out = apply_batch(&files, &[BatchStep::new("drop unused", p)], false).unwrap();
        assert!(out.changed.contains_key("m.py"));
        assert!(!out.changed.contains_key("other.py"));
    }
}

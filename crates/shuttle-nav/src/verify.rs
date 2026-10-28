//! `shuttle verify --run` — axis-2 **execution truth**: actually build + test the T2
//! land-time re-verify set inside the `cell-run` permission cage, then fold each
//! unit's runtime verdict into a machine [`ToolOutcome`]. A Green here means
//! *proven compiled + proven green*, not "statically inferred".
//!
//! # Why this module exists (§7 「绝不瞎猜」, red line #20)
//! `shuttle impact` (T2) computes the minimal set a lander must re-verify — but it
//! only *reasons*; it never runs anything. This module closes that gap by
//! partitioning the re-verify set into test / non-test units and running each
//! test unit's build + test **inside the cage** (never a bare shell — #11). The
//! iron rule: a unit that *did not run* is never laundered into a pass. If a
//! command is not in the cell binding allowlist, or the environment is missing,
//! or a laned side effect (net/clock/global state) is required, the unit is a
//! *blocking skip* → the whole verdict is `needs-run`, never green.
//!
//! # The pure/​IO split (mirrors [`crate::reverify`])
//! [`fold_verify`] is pure and deterministic: given a [`ReverifyDetail`] and an
//! injected [`VerifyRunner`], it produces the [`ToolOutcome`] with zero I/O —
//! this is where every §7 downgrade branch is unit-tested with a scripted
//! runner (no git, no ctags). [`drive_verify`] is the thin I/O wrapper: it runs
//! the select seam ([`crate::reverify::reverify_impact`]) to obtain the T2 set,
//! then hands it to [`fold_verify`]. This is the same shape as
//! `compute_reverify` (pure) + `reverify_impact` (I/O), and the same pure-loop +
//! injected-boundary pattern as [`crate::goal::drive_goal`] / `orchestrate.rs`.
//!
//! # The four seams (§3.1 of the design)
//! ```text
//! select   : reverify_impact → ReverifyDetail (the set + the staleness watermark)
//! partition: split units into test / non-test (reuse `looks_like_test_file`)
//! run      : build + test each test unit in the cage (injected `VerifyRunner`)
//! fold     : per-unit pass/fail/skip → VerifyDetail → verify_outcome
//! ```
//! Only `select` (git + ctags) and `run` (the runner) touch the outside world;
//! every *decision* — what counts as a blocking skip, when partial caps green,
//! how counts are tallied — lives here in tested code.

use shuttle_protocol::{
    skip_reason, verify_kind, verify_outcome, CellBinding, FailedUnit, ReverifyDetail,
    ReverifyUnit, SkippedUnit, ToolOutcome, TypedDetail, VerifyDetail, TOOL_VERIFY,
};

/// Default reverse-dependency closure depth for the select seam, matching the
/// `shuttle impact` CLI default so `shuttle verify` re-verifies exactly the T2 set.
pub const DEFAULT_MAX_DEPTH: usize = 6;

// ---------------------------------------------------------------------------
// The run seam (the only thing besides `select` that touches the outside world).
// ---------------------------------------------------------------------------

/// The verdict of building one compile unit inside the cage.
///
/// A `Built` unit is ready to test; a `Failed` build is *ground truth* (it
/// dominates every softer signal, even a stale index); a `Skipped` build means
/// we could not even attempt it (command not in the allowlist, or the toolchain
/// is missing) — a blocking skip unless it is the benign `no-test`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildResult {
    /// The unit compiled — proceed to [`VerifyRunner::test`].
    Built,
    /// The unit failed to compile; `evidence` is a machine-safe stderr digest.
    Failed { evidence: String },
    /// The build was not attempted; `reason` is one of [`skip_reason`].
    Skipped { reason: String },
}

/// The verdict of running one unit's test inside the cage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestResult {
    /// The unit's test(s) passed.
    Passed,
    /// A test was red; `evidence` is the first failing assertion / digest.
    Failed { evidence: String },
    /// The test was not run; `reason` is one of [`skip_reason`] (e.g.
    /// `env-missing` when a hermetic run is impossible in the cage).
    Skipped { reason: String },
}

/// The single boundary [`fold_verify`] delegates to the outside world: build,
/// then (if it built) test, one compile unit inside the `cell-run` cage. The
/// `cage` gives the impl the allowlist / workspace root it must honor — a
/// command not in `cage.allowed_commands` MUST come back as
/// `Skipped { reason: not-allowed }`, never spawned (§7, #11). Real production
/// wires this to [`crate::verify::CellVerifyRunner`] (built on `run_cell`);
/// tests inject a scripted runner. Every *decision* around these calls stays in
/// [`fold_verify`], not in the impl.
pub trait VerifyRunner {
    /// Build one compile unit inside `cage`. Returning
    /// [`BuildResult::Skipped`] with a blocking reason forces `needs-run`.
    fn build(&mut self, unit: &ReverifyUnit, cage: &CellBinding) -> BuildResult;

    /// Run one built unit's test inside `cage`. Only called after
    /// [`VerifyRunner::build`] returned [`BuildResult::Built`].
    fn test(&mut self, unit: &ReverifyUnit, cage: &CellBinding) -> TestResult;
}

// ---------------------------------------------------------------------------
// The pure fold (no I/O, deterministic given the runner).
// ---------------------------------------------------------------------------

/// Partition + run + fold: turn a T2 [`ReverifyDetail`] into an execution-truth
/// [`ToolOutcome`], running each *test* unit's build + test through `runner`.
///
/// Pure except for the `runner` seam. Deterministic order (the units come
/// pre-sorted by `compute_reverify`). Non-test units are benign `no-test` skips
/// (nothing to run) and never block; a test unit blocks the whole verdict when
/// its build/test is skipped for an allowlist / environment reason (§7).
///
/// `frontier_partial` is the **selection-completeness** caveat: the changed
/// symbols had an outgoing *UNKNOWN* symbol-graph edge (e.g. an A5 dynamic
/// import) that the trusted directed closure deliberately did **not** follow,
/// so the re-verify set may miss a consumer. When true, even an all-green run
/// is capped at `Green_low` (`partial`), never full green — we do not claim to
/// have covered a frontier we cannot prove. `reverify_impact` follows only
/// directed trusted edges and therefore cannot surface this itself; until the
/// select stage carries it natively (G6 semantic-contract graph) the caller
/// supplies it, defaulting to a conservative `false`. A conservative `false`
/// never fabricates a *stronger* claim than we ran — it asserts "no UNKNOWN
/// frontier was detected", which is exactly what a directed-only closure means.
pub fn fold_verify<R: VerifyRunner>(
    rd: &ReverifyDetail,
    frontier_partial: bool,
    cage: &CellBinding,
    runner: &mut R,
) -> ToolOutcome {
    let mut ran = 0usize;
    let mut passed = 0usize;
    let mut failed: Vec<FailedUnit> = Vec::new();
    let mut skipped: Vec<SkippedUnit> = Vec::new();

    for unit in &rd.reverify {
        // partition: a non-test compile unit has nothing to run — a benign,
        // non-blocking `no-test` skip (never forces needs-run).
        if !crate::looks_like_test_file(&unit.path) {
            skipped.push(SkippedUnit {
                unit: unit.path.clone(),
                reason: skip_reason::NO_TEST.to_string(),
            });
            continue;
        }

        // run: build first — a failed build is ground truth and skips the test.
        match runner.build(unit, cage) {
            BuildResult::Skipped { reason } => {
                skipped.push(SkippedUnit {
                    unit: unit.path.clone(),
                    reason,
                });
            }
            BuildResult::Failed { evidence } => {
                ran += 1;
                failed.push(FailedUnit {
                    unit: unit.path.clone(),
                    kind: verify_kind::BUILD.to_string(),
                    evidence,
                });
            }
            BuildResult::Built => match runner.test(unit, cage) {
                TestResult::Passed => {
                    ran += 1;
                    passed += 1;
                }
                TestResult::Failed { evidence } => {
                    ran += 1;
                    failed.push(FailedUnit {
                        unit: unit.path.clone(),
                        kind: verify_kind::TEST.to_string(),
                        evidence,
                    });
                }
                TestResult::Skipped { reason } => {
                    // It compiled but could not be run hermetically — a blocking
                    // skip (unless `no-test`). We never call the built-but-unrun
                    // unit green.
                    skipped.push(SkippedUnit {
                        unit: unit.path.clone(),
                        reason,
                    });
                }
            },
        }
    }

    let detail = VerifyDetail {
        base_oid: rd.base_oid.clone(),
        head_oid: rd.head_oid.clone(),
        reverify_size: rd.reverify_size,
        ran,
        passed,
        failed,
        skipped,
        partial: frontier_partial,
        // Transpose the T2 staleness watermark: a stale graph means the set
        // itself may be incomplete, so even an all-green run blocks stale-index.
        degraded: rd.degraded,
    };
    verify_outcome(detail)
}

// ---------------------------------------------------------------------------
// The I/O wrapper (the shape the CLI speaks).
// ---------------------------------------------------------------------------

/// Drive the full `shuttle verify --run` state machine for `base..head` over
/// `opts.root`, delegating the run to `runner` inside `cage`.
///
/// select (`reverify_impact`) → partition + run + fold (`fold_verify`). A bad
/// revspec / non-repo root surfaces as an `Error` [`ToolOutcome`] re-stamped
/// under [`TOOL_VERIFY`] (so the CLI always has a machine envelope); a genuine
/// scan failure propagates as [`crate::NavError`]. An empty diff short-circuits
/// through `reverify_impact` to an honest Green (nothing to run, nothing
/// withheld). A stale index degrades to `stale-index` via the folded
/// `degraded` watermark — shuttle never runs a "green" off a stale graph (C11, §7).
///
/// See [`fold_verify`] for the meaning of `frontier_partial`.
pub fn drive_verify<R: VerifyRunner>(
    base: &str,
    head: &str,
    max_depth: usize,
    frontier_partial: bool,
    opts: &crate::QueryOptions,
    cage: &CellBinding,
    runner: &mut R,
) -> Result<ToolOutcome, crate::NavError> {
    // select: reuse T2 verbatim — same set, same staleness watermark.
    let select = crate::reverify::reverify_impact(base, head, max_depth, opts)?;
    match select.detail {
        TypedDetail::Reverify(rd) => Ok(fold_verify(&rd, frontier_partial, cage, runner)),
        // reverify_impact only ever returns a Reverify detail on success; any
        // other detail is an Error envelope (bad revspec / io). Re-stamp it
        // under `verify` so the tool name is honest, preserving the reason.
        _ => Ok(ToolOutcome::error(
            TOOL_VERIFY,
            select.reason_code,
            select.summary.unwrap_or_default(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuttle_protocol::{OutcomeConfidence, OutcomeState};
    use std::collections::BTreeMap;

    // --- builders ---------------------------------------------------------

    /// A minimal, well-formed cage. `fold_verify` passes it to the runner
    /// opaquely, so the scripted runner ignores it; it exists only to satisfy
    /// the trait signature.
    fn cage() -> CellBinding {
        CellBinding {
            version: shuttle_protocol::CELL_BINDING_VERSION,
            workspace_root: "/w".to_string(),
            allowed_paths: vec!["**".to_string()],
            allowed_commands: Vec::new(),
            base_oid: "base".to_string(),
            assignment_digest: "sha256:0".to_string(),
            runtime_descriptor_ref: None,
            runtime_descriptor_digest: None,
            runtime_descriptor: None,
            shuttle_binary_digest: None,
            environment_id: "env".to_string(),
            provider_run_id: None,
            attempt_id: None,
            runtime_id: None,
            package_id: None,
            profile_ref: None,
            session_file: None,
            repo_set: None,
            project: Default::default(),
        }
    }

    fn unit(path: &str) -> ReverifyUnit {
        ReverifyUnit {
            path: path.to_string(),
            reason: "changed".to_string(),
            confidence: OutcomeConfidence::High,
            distance: 0,
        }
    }

    /// A `ReverifyDetail` over `paths` (all "changed"), with an explicit
    /// staleness watermark.
    fn rdetail(paths: &[&str], degraded: bool) -> ReverifyDetail {
        let reverify: Vec<ReverifyUnit> = paths.iter().map(|p| unit(p)).collect();
        ReverifyDetail {
            base_oid: "base".to_string(),
            head_oid: "head".to_string(),
            index_base_oid: if degraded { "stale" } else { "head" }.to_string(),
            changed_files: paths.iter().map(|p| p.to_string()).collect(),
            reverify: reverify.clone(),
            granularity: "file".to_string(),
            degraded,
            reverify_size: reverify.len(),
        }
    }

    // --- the scripted runner ---------------------------------------------

    /// A `VerifyRunner` whose build/test verdicts are canned per unit path, so
    /// `fold_verify` can be exercised with zero git / ctags / process spawns.
    /// Records the order of build/test calls for ordering assertions.
    #[derive(Default)]
    struct ScriptRunner {
        build: BTreeMap<String, BuildResult>,
        test: BTreeMap<String, TestResult>,
        built: Vec<String>,
        tested: Vec<String>,
    }

    impl ScriptRunner {
        fn on_build(mut self, path: &str, r: BuildResult) -> Self {
            self.build.insert(path.to_string(), r);
            self
        }
        fn on_test(mut self, path: &str, r: TestResult) -> Self {
            self.test.insert(path.to_string(), r);
            self
        }
    }

    impl VerifyRunner for ScriptRunner {
        fn build(&mut self, u: &ReverifyUnit, _cage: &CellBinding) -> BuildResult {
            self.built.push(u.path.clone());
            self.build
                .get(&u.path)
                .cloned()
                .unwrap_or(BuildResult::Built)
        }
        fn test(&mut self, u: &ReverifyUnit, _cage: &CellBinding) -> TestResult {
            self.tested.push(u.path.clone());
            self.test
                .get(&u.path)
                .cloned()
                .unwrap_or(TestResult::Passed)
        }
    }

    fn verify_detail(o: &ToolOutcome) -> &VerifyDetail {
        match &o.detail {
            TypedDetail::Verify(d) => d,
            other => panic!("expected Verify detail, got {other:?}"),
        }
    }

    // --- §7 downgrade matrix (one dedicated assertion per row) ------------

    #[test]
    fn green_when_all_affected_tests_build_and_pass() {
        let rd = rdetail(&["a_test.py", "b_test.py"], false);
        let mut r = ScriptRunner::default();
        let o = fold_verify(&rd, false, &cage(), &mut r);
        assert_eq!(o.state, OutcomeState::Green);
        assert_eq!(o.reason_code, shuttle_protocol::reason::OK);
        let d = verify_detail(&o);
        assert_eq!(d.ran, 2);
        assert_eq!(d.passed, 2);
        assert!(d.failed.is_empty());
        assert!(d.skipped.is_empty());
        assert!(!d.partial);
        // build precedes test, in the set's deterministic order.
        assert_eq!(r.built, vec!["a_test.py", "b_test.py"]);
        assert_eq!(r.tested, vec!["a_test.py", "b_test.py"]);
    }

    #[test]
    fn build_failure_blocks_build_blocked_with_evidence_and_skips_its_test() {
        let rd = rdetail(&["a_test.py"], false);
        let mut r = ScriptRunner::default().on_build(
            "a_test.py",
            BuildResult::Failed {
                evidence: "E0499: cannot borrow".to_string(),
            },
        );
        let o = fold_verify(&rd, false, &cage(), &mut r);
        assert_eq!(o.state, OutcomeState::Blocked);
        assert_eq!(o.reason_code, shuttle_protocol::reason::BUILD_BLOCKED);
        assert!(o.whats_missing.iter().any(|m| m.contains("E0499")));
        // a failed build must NOT proceed to test.
        assert!(r.tested.is_empty());
        let d = verify_detail(&o);
        assert_eq!(d.ran, 1);
        assert_eq!(d.passed, 0);
        assert_eq!(d.failed.len(), 1);
        assert_eq!(d.failed[0].kind, verify_kind::BUILD);
    }

    #[test]
    fn test_failure_blocks_test_blocked_with_evidence() {
        let rd = rdetail(&["a_test.py"], false);
        let mut r = ScriptRunner::default().on_test(
            "a_test.py",
            TestResult::Failed {
                evidence: "assert_eq! left != right".to_string(),
            },
        );
        let o = fold_verify(&rd, false, &cage(), &mut r);
        assert_eq!(o.state, OutcomeState::Blocked);
        assert_eq!(o.reason_code, shuttle_protocol::reason::TEST_BLOCKED);
        assert!(o.whats_missing.iter().any(|m| m.contains("left != right")));
        let d = verify_detail(&o);
        assert_eq!(d.ran, 1);
        assert_eq!(d.failed[0].kind, verify_kind::TEST);
    }

    #[test]
    fn allowlist_miss_blocks_needs_run_never_green() {
        // A build the runner refuses because the command is not authorized.
        let rd = rdetail(&["a_test.py"], false);
        let mut r = ScriptRunner::default().on_build(
            "a_test.py",
            BuildResult::Skipped {
                reason: skip_reason::NOT_ALLOWED.to_string(),
            },
        );
        let o = fold_verify(&rd, false, &cage(), &mut r);
        assert_eq!(o.state, OutcomeState::Blocked);
        assert_eq!(o.reason_code, shuttle_protocol::reason::NEEDS_RUN);
        let d = verify_detail(&o);
        assert_eq!(d.ran, 0);
        assert_eq!(d.skipped.len(), 1);
        assert_eq!(d.skipped[0].reason, skip_reason::NOT_ALLOWED);
    }

    #[test]
    fn env_missing_at_test_stage_blocks_needs_run() {
        // Built fine, but a hermetic run is impossible (net/clock/global state).
        let rd = rdetail(&["a_test.py"], false);
        let mut r = ScriptRunner::default().on_test(
            "a_test.py",
            TestResult::Skipped {
                reason: skip_reason::ENV_MISSING.to_string(),
            },
        );
        let o = fold_verify(&rd, false, &cage(), &mut r);
        assert_eq!(o.state, OutcomeState::Blocked);
        assert_eq!(o.reason_code, shuttle_protocol::reason::NEEDS_RUN);
        let d = verify_detail(&o);
        assert_eq!(d.ran, 0);
        assert_eq!(d.skipped[0].reason, skip_reason::ENV_MISSING);
    }

    #[test]
    fn unknown_frontier_caps_all_green_run_at_green_low_partial() {
        let rd = rdetail(&["a_test.py"], false);
        let mut r = ScriptRunner::default();
        let o = fold_verify(&rd, /* frontier_partial */ true, &cage(), &mut r);
        assert_eq!(o.state, OutcomeState::Green);
        assert_eq!(o.reason_code, shuttle_protocol::reason::OK_LOW_CONFIDENCE);
        let d = verify_detail(&o);
        assert!(d.partial);
        assert_eq!(d.passed, 1);
    }

    #[test]
    fn stale_index_blocks_stale_index_even_when_units_pass() {
        let rd = rdetail(&["a_test.py"], /* degraded */ true);
        let mut r = ScriptRunner::default();
        let o = fold_verify(&rd, false, &cage(), &mut r);
        assert_eq!(o.state, OutcomeState::Blocked);
        assert_eq!(o.reason_code, shuttle_protocol::reason::STALE_INDEX);
        let d = verify_detail(&o);
        // it still ran (ground-truth first), but the stale watermark blocks green.
        assert_eq!(d.passed, 1);
        assert!(d.degraded);
    }

    #[test]
    fn empty_diff_is_honest_green() {
        let rd = rdetail(&[], false);
        let mut r = ScriptRunner::default();
        let o = fold_verify(&rd, false, &cage(), &mut r);
        assert_eq!(o.state, OutcomeState::Green);
        assert_eq!(o.reason_code, shuttle_protocol::reason::OK);
        let d = verify_detail(&o);
        assert_eq!(d.ran, 0);
        assert!(d.skipped.is_empty());
        assert!(r.built.is_empty());
    }

    // --- precedence + partition edge cases --------------------------------

    #[test]
    fn build_failure_dominates_stale_index() {
        // Ground truth (a real compile failure) outranks the staleness caveat.
        let rd = rdetail(&["a_test.py"], /* degraded */ true);
        let mut r = ScriptRunner::default().on_build(
            "a_test.py",
            BuildResult::Failed {
                evidence: "syntax error".to_string(),
            },
        );
        let o = fold_verify(&rd, false, &cage(), &mut r);
        assert_eq!(o.reason_code, shuttle_protocol::reason::BUILD_BLOCKED);
    }

    #[test]
    fn needs_run_dominates_stale_index() {
        // "could not run" outranks "stale graph": both are non-green, but an
        // un-run unit is the sharper "we did not prove it" (§7).
        let rd = rdetail(&["a_test.py"], /* degraded */ true);
        let mut r = ScriptRunner::default().on_build(
            "a_test.py",
            BuildResult::Skipped {
                reason: skip_reason::NOT_ALLOWED.to_string(),
            },
        );
        let o = fold_verify(&rd, false, &cage(), &mut r);
        assert_eq!(o.reason_code, shuttle_protocol::reason::NEEDS_RUN);
    }

    #[test]
    fn non_test_units_are_benign_no_test_skips_and_do_not_block() {
        // A changed non-test source file has nothing to run; paired with a
        // green test unit the whole verdict is still Green.
        let rd = rdetail(&["src/lib.rs", "a_test.py"], false);
        let mut r = ScriptRunner::default();
        let o = fold_verify(&rd, false, &cage(), &mut r);
        assert_eq!(o.state, OutcomeState::Green);
        let d = verify_detail(&o);
        assert_eq!(d.ran, 1); // only the test unit ran
        assert_eq!(d.passed, 1);
        assert_eq!(d.skipped.len(), 1);
        assert_eq!(d.skipped[0].reason, skip_reason::NO_TEST);
        assert_eq!(d.skipped[0].unit, "src/lib.rs");
        // the non-test unit is never built.
        assert_eq!(r.built, vec!["a_test.py"]);
    }

    #[test]
    fn only_no_test_skips_still_green() {
        // Every changed unit is a non-test source file → nothing to run, but
        // nothing is withheld either → honest Green (no blocking skip).
        let rd = rdetail(&["src/a.rs", "src/b.rs"], false);
        let mut r = ScriptRunner::default();
        let o = fold_verify(&rd, false, &cage(), &mut r);
        assert_eq!(o.state, OutcomeState::Green);
        let d = verify_detail(&o);
        assert_eq!(d.ran, 0);
        assert_eq!(d.skipped.len(), 2);
        assert!(d.skipped.iter().all(|s| s.reason == skip_reason::NO_TEST));
    }

    #[test]
    fn build_failure_outranks_a_later_test_failure_in_reporting() {
        // Two failing units: a build fail and a test fail. verify_outcome
        // surfaces build-blocked first (the compile error is the root cause).
        let rd = rdetail(&["a_test.py", "b_test.py"], false);
        let mut r = ScriptRunner::default()
            .on_build(
                "a_test.py",
                BuildResult::Failed {
                    evidence: "no method".to_string(),
                },
            )
            .on_test(
                "b_test.py",
                TestResult::Failed {
                    evidence: "boom".to_string(),
                },
            );
        let o = fold_verify(&rd, false, &cage(), &mut r);
        assert_eq!(o.reason_code, shuttle_protocol::reason::BUILD_BLOCKED);
        let d = verify_detail(&o);
        assert_eq!(d.failed.len(), 2); // both recorded in the detail
    }

    #[test]
    fn counts_are_exact_across_a_mixed_set() {
        // pass + build-fail + not-allowed-skip + non-test → ran=1 passed=1
        // failed=1 skipped=2 (one no-test, one not-allowed).
        let rd = rdetail(&["p_test.py", "f_test.py", "s_test.py", "src/x.rs"], false);
        let mut r = ScriptRunner::default()
            .on_build(
                "f_test.py",
                BuildResult::Failed {
                    evidence: "e".to_string(),
                },
            )
            .on_build(
                "s_test.py",
                BuildResult::Skipped {
                    reason: skip_reason::NOT_ALLOWED.to_string(),
                },
            );
        let o = fold_verify(&rd, false, &cage(), &mut r);
        let d = verify_detail(&o);
        assert_eq!(d.ran, 2); // p (built+tested) + f (built, failed)
        assert_eq!(d.passed, 1);
        assert_eq!(d.failed.len(), 1);
        assert_eq!(d.skipped.len(), 2);
        // reverify_size echoes the paper T2 cardinality, independent of run.
        assert_eq!(d.reverify_size, 4);
        // a build failure is present → build-blocked wins by precedence over
        // the blocking skip and the pass (ground truth dominates).
        assert_eq!(o.reason_code, shuttle_protocol::reason::BUILD_BLOCKED);
    }
}

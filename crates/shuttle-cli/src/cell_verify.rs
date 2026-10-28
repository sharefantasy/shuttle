//! `CellVerifyRunner` — the real-world [`VerifyRunner`] for `shuttle verify --run`.
//!
//! [`shuttle_nav::verify::fold_verify`] owns every *decision* (what blocks, when
//! partial caps green, how counts tally); this module owns only the two
//! outside-world touches: pick the language-native build/test command for a
//! compile unit, and spawn it **inside the cage**. The cage is two gates, both
//! fail-closed:
//!
//!   1. **allowlist** — a command spawns only if the cell binding's
//!      [`CellBinding::command_allowed`] authorizes its argv. A miss is a
//!      `not-allowed` skip → the whole verdict is `needs-run`; shuttle never spawns
//!      an unauthorized command (§7, #11).
//!   2. **confinement** — the child runs with its working directory pinned to
//!      the binding's `workspace_root` (the same root the mandatory
//!      [`ScopedPathResolver`] guards), so a build/test cannot escape the cell.
//!
//! A unit whose language shuttle has **no** native build/test recipe for is an
//! `env-missing` skip (blocking) — we do not know how to run it hermetically,
//! so per §7 we refuse to call it green rather than guess.
//!
//! # Injectable process boundary
//! The spawn itself is the [`CageSpawner`] seam. Production wires
//! [`ProcSpawner`] (a confined [`std::process::Command`]); tests inject a
//! scripted spawner so the command-selection + gate logic is exercised with
//! zero real toolchains.

use std::path::{Path, PathBuf};

use shuttle_nav::verify::{BuildResult, TestResult, VerifyRunner};
use shuttle_protocol::{skip_reason, CellBinding, ReverifyUnit};

/// The result of running one caged command: the child's exit code and a
/// machine-safe stderr digest (already length-capped) used as failure evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawned {
    /// Child process exit code (0 = success).
    pub code: i32,
    /// Truncated stderr digest, safe to surface as `FailedUnit::evidence`.
    pub evidence: String,
}

/// The single process boundary [`CellVerifyRunner`] delegates to. Real runs use
/// [`ProcSpawner`]; tests inject a scripted impl so command selection + the
/// allowlist gate are unit-testable without invoking cargo/go/pytest.
pub trait CageSpawner {
    /// Spawn `argv` with the working directory pinned to `cwd`. An `Err` means
    /// the command could not be launched at all (missing toolchain) — the
    /// caller maps that to an `env-missing` skip, never a pass.
    fn spawn(&mut self, argv: &[String], cwd: &Path) -> std::io::Result<Spawned>;
}

/// Cap a stderr digest so evidence stays machine-safe and bounded. This is a
/// runner invariant: applied in [`CellVerifyRunner`] itself (not in the
/// spawner), so the bound holds regardless of which [`CageSpawner`] is injected.
fn cap_evidence(raw: &str) -> String {
    const MAX: usize = 400;
    let trimmed = raw.trim();
    if trimmed.chars().count() <= MAX {
        trimmed.to_string()
    } else {
        // Keep the tail — the actual error usually sits at the end of a build log.
        let tail: String = trimmed
            .chars()
            .rev()
            .take(MAX)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("…{tail}")
    }
}

/// Production spawner: a [`std::process::Command`] confined to `cwd`, capturing
/// stderr for the evidence digest. It never inherits a shell — argv is executed
/// directly, so there is no shell-injection surface.
pub struct ProcSpawner;

impl CageSpawner for ProcSpawner {
    fn spawn(&mut self, argv: &[String], cwd: &Path) -> std::io::Result<Spawned> {
        let (program, rest) = argv
            .split_first()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty argv"))?;
        let out = std::process::Command::new(program)
            .args(rest)
            .current_dir(cwd)
            .output()?;
        Ok(Spawned {
            code: out.status.code().unwrap_or(-1),
            evidence: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// The language-native build/test invocation for a compile unit. `build` is
/// `None` for an interpreted language (nothing to compile — trivially built).
struct LangCommands {
    build: Option<Vec<String>>,
    test: Vec<String>,
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Map a unit path's extension to shuttle's native build/test recipe. Returns `None`
/// for a language shuttle cannot verify natively — the caller turns that into an
/// `env-missing` skip (blocking), never a fabricated green (§7). The argv is
/// the toolchain's canonical affected-subset invocation (crate/package level for
/// compiled languages); per-file granularity is the *selection*'s job (T2), the
/// command's job is to prove the unit's crate/package still builds + tests green.
fn commands_for(path: &str) -> Option<LangCommands> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "rs" => Some(LangCommands {
            build: Some(argv(&["cargo", "build"])),
            test: argv(&["cargo", "test"]),
        }),
        "go" => Some(LangCommands {
            build: Some(argv(&["go", "build", "./..."])),
            test: argv(&["go", "test", "./..."]),
        }),
        "py" => Some(LangCommands {
            build: None,
            test: argv(&["pytest"]),
        }),
        "js" | "jsx" | "ts" | "tsx" => Some(LangCommands {
            build: None,
            test: argv(&["npm", "test"]),
        }),
        _ => None,
    }
}

/// The concrete [`VerifyRunner`]: build/test each unit inside the cage.
pub struct CellVerifyRunner<S: CageSpawner> {
    root: PathBuf,
    spawner: S,
}

impl<S: CageSpawner> CellVerifyRunner<S> {
    /// Construct a runner confined to `workspace_root`, spawning through
    /// `spawner`. `workspace_root` should be the (canonicalized) binding root
    /// the [`ScopedPathResolver`](shuttle_protocol::ScopedPathResolver) guards.
    pub fn new(workspace_root: impl Into<PathBuf>, spawner: S) -> Self {
        CellVerifyRunner {
            root: workspace_root.into(),
            spawner,
        }
    }
}

/// Convenience constructor for the production spawner.
impl CellVerifyRunner<ProcSpawner> {
    /// A runner that spawns real confined subprocesses.
    pub fn confined(workspace_root: impl Into<PathBuf>) -> Self {
        CellVerifyRunner::new(workspace_root, ProcSpawner)
    }
}

impl<S: CageSpawner> VerifyRunner for CellVerifyRunner<S> {
    fn build(&mut self, unit: &ReverifyUnit, cage: &CellBinding) -> BuildResult {
        let Some(cmds) = commands_for(&unit.path) else {
            // No native recipe → we cannot verify it hermetically. Blocking.
            return BuildResult::Skipped {
                reason: skip_reason::ENV_MISSING.to_string(),
            };
        };
        let Some(build_argv) = cmds.build else {
            // Interpreted language: nothing to compile — trivially built.
            return BuildResult::Built;
        };
        if !cage.command_allowed(&build_argv) {
            return BuildResult::Skipped {
                reason: skip_reason::NOT_ALLOWED.to_string(),
            };
        }
        match self.spawner.spawn(&build_argv, &self.root) {
            Ok(s) if s.code == 0 => BuildResult::Built,
            Ok(s) => BuildResult::Failed {
                evidence: cap_evidence(&s.evidence),
            },
            // Toolchain absent / could not launch — an environment gap, not a
            // failure we can attribute to the code. Blocking skip (§7).
            Err(e) => BuildResult::Skipped {
                reason: format!("{}: {e}", skip_reason::ENV_MISSING),
            },
        }
    }

    fn test(&mut self, unit: &ReverifyUnit, cage: &CellBinding) -> TestResult {
        let Some(cmds) = commands_for(&unit.path) else {
            return TestResult::Skipped {
                reason: skip_reason::ENV_MISSING.to_string(),
            };
        };
        if !cage.command_allowed(&cmds.test) {
            return TestResult::Skipped {
                reason: skip_reason::NOT_ALLOWED.to_string(),
            };
        }
        match self.spawner.spawn(&cmds.test, &self.root) {
            Ok(s) if s.code == 0 => TestResult::Passed,
            Ok(s) => TestResult::Failed {
                evidence: cap_evidence(&s.evidence),
            },
            Err(e) => TestResult::Skipped {
                reason: format!("{}: {e}", skip_reason::ENV_MISSING),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuttle_protocol::{AllowedCommand, OutcomeConfidence, CELL_BINDING_VERSION};
    use std::collections::VecDeque;

    // --- builders ---------------------------------------------------------

    fn unit(path: &str) -> ReverifyUnit {
        ReverifyUnit {
            path: path.to_string(),
            reason: "changed".to_string(),
            confidence: OutcomeConfidence::High,
            distance: 0,
        }
    }

    /// A cage authorizing exactly the given `(program, argv_prefix)` entries.
    fn cage(cmds: &[(&str, Option<Vec<&str>>)]) -> CellBinding {
        CellBinding {
            version: CELL_BINDING_VERSION,
            workspace_root: "/w".to_string(),
            allowed_paths: vec!["**".to_string()],
            allowed_commands: cmds
                .iter()
                .map(|(p, pre)| AllowedCommand {
                    program: p.to_string(),
                    argv_prefix: pre
                        .as_ref()
                        .map(|v| v.iter().map(|s| s.to_string()).collect()),
                })
                .collect(),
            base_oid: "base".to_string(),
            assignment_digest: "sha256:0".to_string(),
            runtime_descriptor_ref: None,
            runtime_descriptor_digest: None,
            runtime_descriptor: None,
            project: shuttle_protocol::CellProjectBinding::LegacyUnbound,
            shuttle_binary_digest: None,
            environment_id: "env".to_string(),
            provider_run_id: None,
            attempt_id: None,
            runtime_id: None,
            package_id: None,
            profile_ref: None,
            session_file: None,
            repo_set: None,
        }
    }

    /// A scripted spawner: returns canned `Spawned`/`Err` per call in order, and
    /// records the argvs it was asked to run.
    #[derive(Default)]
    struct FakeSpawner {
        queue: VecDeque<std::io::Result<Spawned>>,
        seen: Vec<Vec<String>>,
    }
    impl FakeSpawner {
        fn ok(mut self, code: i32, evidence: &str) -> Self {
            self.queue.push_back(Ok(Spawned {
                code,
                evidence: evidence.to_string(),
            }));
            self
        }
        fn launch_err(mut self) -> Self {
            self.queue.push_back(Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such toolchain",
            )));
            self
        }
    }
    impl CageSpawner for FakeSpawner {
        fn spawn(&mut self, argv: &[String], _cwd: &Path) -> std::io::Result<Spawned> {
            self.seen.push(argv.to_vec());
            self.queue.pop_front().unwrap_or(Ok(Spawned {
                code: 0,
                evidence: String::new(),
            }))
        }
    }

    // --- tests ------------------------------------------------------------

    #[test]
    fn rust_build_and_test_pass_when_allowed() {
        let mut r = CellVerifyRunner::new("/w", FakeSpawner::default().ok(0, "").ok(0, ""));
        let c = cage(&[("cargo", None)]);
        assert_eq!(r.build(&unit("src/a_test.rs"), &c), BuildResult::Built);
        assert_eq!(r.test(&unit("src/a_test.rs"), &c), TestResult::Passed);
        assert_eq!(r.spawner.seen[0], vec!["cargo", "build"]);
        assert_eq!(r.spawner.seen[1], vec!["cargo", "test"]);
    }

    #[test]
    fn build_failure_carries_evidence() {
        let mut r = CellVerifyRunner::new("/w", FakeSpawner::default().ok(101, "E0433 boom"));
        let c = cage(&[("cargo", None)]);
        match r.build(&unit("x_test.rs"), &c) {
            BuildResult::Failed { evidence } => assert!(evidence.contains("E0433")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn test_failure_carries_evidence() {
        let mut r = CellVerifyRunner::new("/w", FakeSpawner::default().ok(1, "assertion failed"));
        let c = cage(&[("cargo", None)]);
        match r.test(&unit("x_test.rs"), &c) {
            TestResult::Failed { evidence } => assert!(evidence.contains("assertion")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn command_not_in_allowlist_is_not_allowed_skip() {
        // cage authorizes only `go`, but the unit is rust → cargo not allowed.
        let mut r = CellVerifyRunner::new("/w", FakeSpawner::default());
        let c = cage(&[("go", None)]);
        assert_eq!(
            r.build(&unit("x_test.rs"), &c),
            BuildResult::Skipped {
                reason: skip_reason::NOT_ALLOWED.to_string()
            }
        );
        // nothing was spawned — the gate refused before the boundary.
        assert!(r.spawner.seen.is_empty());
    }

    #[test]
    fn argv_prefix_narrows_the_gate() {
        // Authorize `cargo build` but NOT `cargo test`.
        let mut r = CellVerifyRunner::new("/w", FakeSpawner::default().ok(0, ""));
        let c = cage(&[("cargo", Some(vec!["build"]))]);
        assert_eq!(r.build(&unit("x_test.rs"), &c), BuildResult::Built);
        assert_eq!(
            r.test(&unit("x_test.rs"), &c),
            TestResult::Skipped {
                reason: skip_reason::NOT_ALLOWED.to_string()
            }
        );
    }

    #[test]
    fn interpreted_language_is_trivially_built_then_tested() {
        let mut r = CellVerifyRunner::new("/w", FakeSpawner::default().ok(0, ""));
        let c = cage(&[("pytest", None)]);
        // No build spawn for python.
        assert_eq!(r.build(&unit("test_x.py"), &c), BuildResult::Built);
        assert_eq!(r.test(&unit("test_x.py"), &c), TestResult::Passed);
        assert_eq!(r.spawner.seen, vec![vec!["pytest".to_string()]]);
    }

    #[test]
    fn unknown_language_is_env_missing_blocking_skip() {
        let mut r = CellVerifyRunner::new("/w", FakeSpawner::default());
        let c = cage(&[("cargo", None)]);
        assert_eq!(
            r.build(&unit("weird.thrift"), &c),
            BuildResult::Skipped {
                reason: skip_reason::ENV_MISSING.to_string()
            }
        );
        assert!(r.spawner.seen.is_empty());
    }

    #[test]
    fn missing_toolchain_launch_error_is_env_missing_not_a_pass() {
        let mut r = CellVerifyRunner::new("/w", FakeSpawner::default().launch_err());
        let c = cage(&[("cargo", None)]);
        match r.build(&unit("x_test.rs"), &c) {
            BuildResult::Skipped { reason } => {
                assert!(reason.starts_with(skip_reason::ENV_MISSING))
            }
            other => panic!("expected env-missing skip, got {other:?}"),
        }
    }

    #[test]
    fn evidence_digest_caps_length_and_keeps_the_tail() {
        let long = "x".repeat(1000) + "REAL_ERROR_AT_END";
        let mut r = CellVerifyRunner::new("/w", FakeSpawner::default().ok(1, &long));
        let c = cage(&[("cargo", None)]);
        match r.test(&unit("x_test.rs"), &c) {
            TestResult::Failed { evidence } => {
                assert!(evidence.len() < 500);
                assert!(evidence.contains("REAL_ERROR_AT_END"));
                assert!(evidence.starts_with('…'));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}

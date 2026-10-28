//! `shuttle goal` — the CLI wiring for the §4 GoalGraph outer loop (G3).
//!
//! The pure state machine lives in [`shuttle_nav::goal::drive_goal`]; this module is
//! only the four injected seams made concrete against the real world, mirroring
//! how [`crate::run`] wraps pi and how `run_split`/`run_impact` wrap the shuttle-nav
//! tools. The command:
//!
//! ```text
//! shuttle goal (--goal "<text>" | --plan intent.json) [--root R]
//!         [--base <oid> --head <oid>] [--dry-run] [--json] [--telemetry]
//! ```
//!
//! # The four seams (see [`shuttle_nav::goal::GoalRunner`])
//! * **plan** — the pi→shuttle `intent/v1` contract (§6.4). Either read verbatim from
//!   `--plan <file>` (a external-/human-/pre-run-produced DAG) or produced live by
//!   spawning pi in goal-mode (`--goal <text>`): shuttle hands the child an
//!   `SHUTTLE_INTENT_OUT` path, the goal-mode prompt instructs pi to write the
//!   `intent/v1` JSON there, and shuttle reads it back after the run. shuttle embeds no
//!   LLM — planning stays in the one pi model (§9 fair A/B).
//! * **adjudicate** — [`shuttle_nav::split::adjudicate_intent`] (T1 + T3), the
//!   zero-model three-state GoalGraph. Default trait impl.
//! * **execute** — per task, in topological order, a real `shuttle run` supervised
//!   pi loop ([`crate::run::supervise`]); its [`crate::run::RunTelemetry`]
//!   `apply_total`/`apply_ok` map straight onto `proposed`/`accepted` with no
//!   re-derivation (§7 never-invent). `--dry-run` swaps in a no-op executor so
//!   the whole loop is exercisable with zero model calls (the hermetic path).
//! * **reverify** — [`shuttle_nav::reverify::reverify_impact`] (T2) when
//!   `--base/--head` are given; `None` otherwise (no land occurred). shuttle only
//!   computes the minimal re-verify *set*; it never lands (#11).
//!
//! Exit is always 0 (like split/impact): an escalation is a machine-readable
//! result the caller routes on (the `phase`/`escalated`/`review` fields), not a
//! process failure — so pipelines read the envelope, not `$?`.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use shuttle_nav::goal::{
    drive_goal, drive_goal_parallel, drive_goal_resumable, ExecOutput, GoalCheckpoint, GoalResult,
    GoalRunner, TaskExecRequest,
};
use shuttle_nav::parallel::{
    artifact_binding, canonical_digest, correction_digest, schedule_parallel_with, BatchTask,
    ConcurrencyOracle, EitherOracle, FileOverlapOracle, FootprintSource, HybridArtifactBinding,
    HybridFootprintSource, HybridIndexBinding, HybridParallelAdvice as NavHybridParallelAdvice,
    HybridPlanningBasis, HybridRepositoryBase, HybridTaskFootprint, ParallelAdvice,
    ParallelAdviceGraphBasis, ParallelSchedule,
};
use shuttle_nav::reverify::reverify_impact;
use shuttle_nav::split::{
    correct_split, merge_hybrid_dag, split_outcome, HybridEvidenceIssue, HybridEvidenceKind,
    HybridEvidenceState, HybridMechanicalEvidence, HybridMergeInput, HybridTaskEdge,
    HybridWriteClaim, IntentDag,
};
use shuttle_nav::{NavError, QueryOptions};
use shuttle_protocol::{ReconcileVerdict, SplitDetail, ToolOutcome, TypedDetail};

/// Environment variable naming the file the goal-mode pi child must write its
/// `intent/v1` DAG to. The handshake is one-way and file-based so shuttle never has
/// to parse free-form model stdout: pi writes the structured contract, shuttle reads
/// it back (absent/unparseable ⇒ abort, never guess — §7 / #20).
const SHUTTLE_INTENT_OUT: &str = "SHUTTLE_INTENT_OUT";

/// Where the intent DAG comes from — the only thing that differs between the
/// hermetic `--plan` path and the live `--goal` path.
enum PlanSource {
    /// Read an existing `intent/v1` JSON file verbatim (external / human / pre-run).
    File(PathBuf),
    /// Spawn pi in goal-mode to decompose the goal, reading the DAG back from
    /// the `SHUTTLE_INTENT_OUT` file it writes.
    Goal {
        goal: String,
        model: Option<String>,
        thinking: Option<String>,
        provider: Option<String>,
        root: String,
    },
}

/// How a task is executed once the GoalGraph settles Green.
enum ExecMode {
    /// No-op: record zero patches. Exercises the whole outer loop with zero
    /// model calls (the hermetic / preview path).
    DryRun,
    /// Spawn a real `shuttle run` supervised pi loop per task.
    Live {
        model: Option<String>,
        thinking: Option<String>,
        provider: Option<String>,
        root: String,
    },
}

/// How the launcher's *observed* footprint reaches shuttle (P2 drift detection).
/// shuttle owns no launcher (#11) — it never runs the tasks' real edits itself — so
/// the observed footprint is an *input*: either handed in verbatim by whoever
/// did run them (`--observe <map.json>`, a external/launcher/hermetic seam), or, in
/// the live path, read mechanically from the working tree via git (git never
/// lies). Absent ⇒ no observation, and the loop behaves exactly as pre-P2.
enum ObserveMode {
    /// No observation — `observe` returns `None`, the single-pass path.
    None,
    /// A per-task observed-footprint map (`{ "<task-id>": ["<path>", ...] }`),
    /// supplied verbatim by the launcher/external or a hermetic test.
    Injected(HashMap<String, Vec<String>>),
    /// Live: the footprint each task actually left in the working tree,
    /// derived as the *delta* of `git status --porcelain` before vs. after the
    /// task ran (tracked edits + new files). `seen` accumulates across tasks so
    /// each observe attributes only the files that appeared during it.
    Git { root: String },
}

/// The concrete [`GoalRunner`]: the four seams bound to the real world.
struct CliGoalRunner {
    plan_source: PlanSource,
    opts: QueryOptions,
    exec: ExecMode,
    base: Option<String>,
    head: Option<String>,
    max_depth: usize,
    /// P2: where the observed footprint comes from (see [`ObserveMode`]).
    observe: ObserveMode,
    /// Cumulative git-observed changed set, so the live `observe` attributes
    /// only the *new* files that appeared while a given task ran.
    seen: BTreeSet<String>,
    /// The exact intent and symbol table consumed by the most recent
    /// adjudication. Drift re-adjudication overwrites both, so `--advice-out`
    /// can only bind the final corrected schedule, never the stale first pass.
    last_adjudicated_intent: Option<IntentDag>,
    last_base_oid: Option<String>,
    fixed_base_oid: Option<String>,
    /// Exact JSON value read from the frozen `--plan`. `IntentDag` stays
    /// backward-compatible and ignores planning metadata, while v2 advice
    /// binds and validates the full source artifact at this CLI boundary.
    frozen_intent_artifact: Option<serde_json::Value>,
    frozen_planning_revision: Option<u64>,
}

impl CliGoalRunner {
    /// Build a goal-mode decompose prompt: pi produces the structured
    /// `intent/v1` contract (§6.2/§6.4) and writes it to `$SHUTTLE_INTENT_OUT`. Every
    /// dependency it lists is an explicit *guess* — the adjudicator is what
    /// proves it (#20), so the prompt says so.
    fn decompose_prompt(goal: &str) -> String {
        format!(
            "You are in goal-mode. Decompose the following GOAL into a candidate \
task DAG and WRITE IT AS JSON to the file named by the $SHUTTLE_INTENT_OUT \
environment variable. Do not apply any edits.\n\n\
GOAL:\n{goal}\n\n\
Write exactly this schema (intent/v1):\n\
{{\"goal\": \"<goal>\", \"version\": \"intent/v1\", \"tasks\": [\n\
  {{\"id\": \"<stable-id>\", \"intent\": \"<what this task does>\", \
\"touched_files_guess\": [\"<path>\"], \"depends_on_guess\": [\"<id>\"]}}\n\
]}}\n\n\
Rules:\n\
- Every entry in depends_on_guess is a GUESS; shuttle's symbol graph will prove or \
disprove it. Never claim a dependency is certain.\n\
- touched_files_guess are the files each task is expected to change.\n\
- ids must be stable and unique; depends_on_guess references other task ids.\n\
- Write ONLY the JSON to $SHUTTLE_INTENT_OUT (no prose in the file)."
        )
    }

    /// Turn one `shuttle run` [`crate::run::RunTelemetry`] into an [`ExecOutput`],
    /// mapping the raw apply-family counts straight through (§7 never-invent):
    /// `proposed = apply_total`, `accepted = patches = apply_ok`.
    fn exec_output_from_tel(tel: &crate::run::RunTelemetry) -> ExecOutput {
        ExecOutput {
            patches: tel.apply_ok as usize,
            proposed: tel.apply_total as usize,
            accepted: tel.apply_ok as usize,
            tokens: tel.tokens.unwrap_or(0),
            tool_calls: tel.tool_calls as u64,
        }
    }

    /// Spawn pi in goal-mode, capture the `intent/v1` DAG it writes to
    /// `SHUTTLE_INTENT_OUT`, and parse it. Any failure aborts the loop (never guess).
    fn plan_via_pi(
        goal: &str,
        model: &Option<String>,
        thinking: &Option<String>,
        provider: &Option<String>,
        root: &str,
    ) -> Result<IntentDag, NavError> {
        // A per-run scratch file for the child's structured output. Kept beside
        // the repo's `.shuttle/` cache dir so it shares the repo's ignore surface.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let out_path = std::env::temp_dir().join(format!("shuttle_intent_out_{nanos}.json"));

        let mut cfg = crate::config::resolve(
            Self::decompose_prompt(goal),
            model.clone(),
            thinking.clone(),
            provider.clone(),
            // shuttle takes over edit/write in goal-mode: pi only reasons + writes
            // the DAG file via its own bash/write to $SHUTTLE_INTENT_OUT.
            None,
            None,
            root.to_string(),
            false, // deep: default json supervisor
            true,  // no_session: ephemeral decompose
            Vec::new(),
        );
        cfg.child_env.push((
            SHUTTLE_INTENT_OUT.to_string(),
            out_path.to_string_lossy().into_owned(),
        ));

        let _tel = crate::run::supervise(&cfg)
            .map_err(|e| io_err(&format!("goal-mode pi decompose failed: {e}")))?;

        let raw = std::fs::read_to_string(&out_path).map_err(|e| {
            io_err(&format!(
                "pi did not write the intent DAG to {}: {e}",
                out_path.display()
            ))
        })?;
        let _ = std::fs::remove_file(&out_path);
        parse_intent(&raw)
    }
}

impl GoalRunner for CliGoalRunner {
    fn plan(&mut self, _goal: &str) -> Result<IntentDag, NavError> {
        match &self.plan_source {
            PlanSource::File(path) => {
                let raw = std::fs::read_to_string(path)
                    .map_err(|e| io_err(&format!("read intent plan {}: {e}", path.display())))?;
                let artifact: serde_json::Value = serde_json::from_str(&raw)
                    .map_err(|e| io_err(&format!("intent plan is not valid JSON: {e}")))?;
                self.frozen_planning_revision = artifact
                    .get("planning_revision")
                    .and_then(serde_json::Value::as_u64);
                self.frozen_intent_artifact = Some(artifact);
                parse_intent(&raw)
            }
            PlanSource::Goal {
                goal,
                model,
                thinking,
                provider,
                root,
            } => Self::plan_via_pi(goal, model, thinking, provider, root),
        }
    }

    fn adjudicate(&mut self, intent: &IntentDag) -> Result<ToolOutcome, NavError> {
        // Keep the advice provenance on the exact table used for correction.
        // Calling the default and scanning again would allow a filesystem
        // change between scans to produce a false binding.
        let base_before = self
            .fixed_base_oid
            .clone()
            .or_else(|| shuttle_symbols::index::git_head_oid(&self.opts.root));
        let table = shuttle_nav::scan_table(&self.opts)?;
        let base_after = self
            .fixed_base_oid
            .clone()
            .or_else(|| shuttle_symbols::index::git_head_oid(&self.opts.root));
        if base_before != base_after {
            return Err(io_err(
                "git HEAD changed while adjudicating parallel advice; retry on a stable base",
            ));
        }
        let graph =
            shuttle_symbols::graph::SymbolGraph::try_from_scan_root(&table, &self.opts.root)?;
        let detail = correct_split(intent, &graph, &table);
        self.last_adjudicated_intent = Some(intent.clone());
        self.last_base_oid = base_before;
        Ok(split_outcome(detail))
    }

    fn query_options(&self) -> &QueryOptions {
        &self.opts
    }

    fn execute(&mut self, req: &TaskExecRequest) -> Result<ExecOutput, NavError> {
        match &self.exec {
            ExecMode::DryRun => Ok(ExecOutput::default()),
            ExecMode::Live {
                model,
                thinking,
                provider,
                root,
            } => {
                let prompt = task_prompt(req);
                let cfg = crate::config::resolve(
                    prompt,
                    model.clone(),
                    thinking.clone(),
                    provider.clone(),
                    None,
                    None,
                    root.clone(),
                    false,
                    true,
                    Vec::new(),
                );
                let tel = crate::run::supervise(&cfg)
                    .map_err(|e| io_err(&format!("task {} execution failed: {e}", req.id)))?;
                Ok(Self::exec_output_from_tel(&tel))
            }
        }
    }

    fn reverify(&mut self) -> Result<Option<ToolOutcome>, NavError> {
        match (&self.base, &self.head) {
            (Some(base), Some(head)) => {
                let o = reverify_impact(base, head, self.max_depth, &self.opts)?;
                Ok(Some(o))
            }
            _ => Ok(None),
        }
    }

    fn observe(
        &mut self,
        req: &TaskExecRequest,
        _out: &ExecOutput,
    ) -> Result<Option<Vec<String>>, NavError> {
        match &self.observe {
            ObserveMode::None => Ok(None),
            // Verbatim launcher/external/test footprint — never fabricated by shuttle.
            ObserveMode::Injected(map) => Ok(map.get(&req.id).cloned()),
            // Live: the git-observed delta since the previous task. The full
            // current changed set minus everything already attributed to prior
            // tasks is exactly what appeared while this task ran.
            ObserveMode::Git { root } => {
                let current = git_changed_set(root)?;
                let delta: Vec<String> = current
                    .iter()
                    .filter(|f| !self.seen.contains(*f))
                    .cloned()
                    .collect();
                for f in current {
                    self.seen.insert(f);
                }
                Ok(Some(delta))
            }
        }
    }
}

/// The working-tree changed set (`git status --porcelain`): every tracked edit
/// and untracked file, root-relative, matching how the adjudicator names
/// `touched_files`. A git failure is a hard error (never a silent empty set —
/// that would fabricate "no drift"); the caller only reaches here in the live
/// path where the repo is a real git tree.
fn git_changed_set(root: &str) -> Result<BTreeSet<String>, NavError> {
    let git = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git).map_err(|e| {
        io_err(&format!(
            "git resolution for footprint observation failed: {e}"
        ))
    })?;
    let out = std::process::Command::new(git)
        .args(["-C", root, "status", "--porcelain", "--untracked-files=all"])
        .output()
        .map_err(|e| io_err(&format!("git status for footprint observation failed: {e}")))?;
    if !out.status.success() {
        return Err(io_err(&format!(
            "git status for footprint observation failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut set = BTreeSet::new();
    for line in text.lines() {
        // Porcelain v1: two status columns + a space, then the path. A rename
        // is "orig -> new"; the post-rename path is what actually changed.
        if line.len() < 4 {
            continue;
        }
        let path = &line[3..];
        let path = path.rsplit(" -> ").next().unwrap_or(path);
        set.insert(path.trim().to_string());
    }
    Ok(set)
}

/// The symbol-level half of the `--parallel` concurrency oracle (design §2.3).
///
/// The file-overlap oracle already serializes tasks that touch the same file.
/// But two tasks with *disjoint* files can still be coupled through a symbol —
/// e.g. task A defines a function and task B calls it. Running them
/// concurrently would race the definition against its use. This oracle proves
/// (or fails to disprove) that second-order safety mechanically, from shuttle's own
/// symbol graph — never from pi's `touched_files_guess` (§2.3, #20).
///
/// For each task it derives, from the shuttle-corrected `touched` set:
/// * **defines** — the symbol *names defined* in those files (`ctags`), and
/// * **references** — the identifiers in those files that resolve to a symbol
///   the *other* task defines.
///
/// A pair is serialized when they define a common symbol, or when one
/// references a symbol the other defines. Every step that cannot be *proven*
/// (ctags missing, a file unreadable) fails safe by serializing (§7 — the
/// oracle contract forbids returning `None`/"safe" on a hunch).
struct SymbolCouplingOracle {
    root: PathBuf,
    /// task id → its defined-symbol set (`None` = could not be scanned).
    defines: RefCell<HashMap<String, Option<BTreeSet<String>>>>,
    /// task id → the text of each touched file (`None` = a file was unreadable).
    contents: RefCell<HashMap<String, Option<Vec<String>>>>,
}

impl SymbolCouplingOracle {
    fn new(root: &str) -> Self {
        Self {
            root: PathBuf::from(root),
            defines: RefCell::new(HashMap::new()),
            contents: RefCell::new(HashMap::new()),
        }
    }

    /// Resolve a task's touched path against the repo root (touched paths are
    /// repo-relative in the `intent/v1` contract).
    fn resolve(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// The set of symbol names *defined* in a task's touched files, memoized.
    /// `None` means the scan could not be performed (e.g. no ctags backend) —
    /// callers must degrade to serial, never treat it as "defines nothing".
    fn defines_of(&self, task: &BatchTask) -> Option<BTreeSet<String>> {
        if let Some(cached) = self.defines.borrow().get(&task.id) {
            return cached.clone();
        }
        let computed = if task.touched.is_empty() {
            None
        } else {
            let files: Vec<String> = task
                .touched
                .iter()
                .map(|f| self.resolve(f).to_string_lossy().into_owned())
                .collect();
            match shuttle_symbols::scan_files(&files) {
                Ok(tbl) => Some(tbl.symbols.into_iter().map(|s| s.name).collect()),
                Err(_) => None,
            }
        };
        self.defines
            .borrow_mut()
            .insert(task.id.clone(), computed.clone());
        computed
    }

    /// The text of every touched file, memoized. `None` if any file could not
    /// be read (degrade to serial rather than pretend it references nothing).
    fn contents_of(&self, task: &BatchTask) -> Option<Vec<String>> {
        if let Some(cached) = self.contents.borrow().get(&task.id) {
            return cached.clone();
        }
        let mut texts = Vec::with_capacity(task.touched.len());
        let mut ok = true;
        for f in &task.touched {
            match std::fs::read_to_string(self.resolve(f)) {
                Ok(t) => texts.push(t),
                Err(_) => {
                    ok = false;
                    break;
                }
            }
        }
        let computed = if ok { Some(texts) } else { None };
        self.contents
            .borrow_mut()
            .insert(task.id.clone(), computed.clone());
        computed
    }

    /// Does `user` reference any symbol in `defs`? (mechanical identifier scan
    /// filtered to the other task's defined names — never a fuzzy guess.)
    fn references_any(user_texts: &[String], defs: &BTreeSet<String>) -> Option<String> {
        if defs.is_empty() {
            return None;
        }
        let defined: HashSet<&str> = defs.iter().map(|s| s.as_str()).collect();
        for text in user_texts {
            let hits = shuttle_symbols::extract_references(text, &defined);
            if let Some(sym) = hits.into_iter().next() {
                return Some(sym);
            }
        }
        None
    }
}

impl ConcurrencyOracle for SymbolCouplingOracle {
    fn conflict(&self, a: &BatchTask, b: &BatchTask) -> Option<String> {
        // Defined-symbol sets. A missing scan is unprovable ⇒ serialize (§7).
        let da = match self.defines_of(a) {
            Some(d) => d,
            None => {
                return Some(format!(
                    "unprovable: could not scan symbols defined by {}",
                    a.id
                ))
            }
        };
        let db = match self.defines_of(b) {
            Some(d) => d,
            None => {
                return Some(format!(
                    "unprovable: could not scan symbols defined by {}",
                    b.id
                ))
            }
        };

        // Both define the same name — same declaration surface, coupled.
        if let Some(sym) = da.intersection(&db).next() {
            return Some(format!("symbol-coupling: both tasks define `{sym}`"));
        }

        // One references a symbol the other defines — a use↔def coupling the
        // file-overlap oracle cannot see. Reading is likewise fail-safe.
        let ta = match self.contents_of(a) {
            Some(t) => t,
            None => {
                return Some(format!(
                    "unprovable: could not read the files touched by {}",
                    a.id
                ))
            }
        };
        let tb = match self.contents_of(b) {
            Some(t) => t,
            None => {
                return Some(format!(
                    "unprovable: could not read the files touched by {}",
                    b.id
                ))
            }
        };
        if let Some(sym) = Self::references_any(&ta, &db) {
            return Some(format!(
                "symbol-coupling: {} uses `{sym}` defined by {}",
                a.id, b.id
            ));
        }
        if let Some(sym) = Self::references_any(&tb, &da) {
            return Some(format!(
                "symbol-coupling: {} uses `{sym}` defined by {}",
                b.id, a.id
            ));
        }

        // File-disjoint AND symbol-disjoint: provably safe to co-schedule.
        None
    }
}

/// Compose the per-task pi prompt from the corrected GoalGraph node: intent,
/// the files it owns, and the prerequisites already landed (so the executor can
/// assemble the upstream contract via `shuttle context-pack`).
fn task_prompt(req: &TaskExecRequest) -> String {
    let files = if req.touched_files.is_empty() {
        "(none specified)".to_string()
    } else {
        req.touched_files.join(", ")
    };
    let prereqs = if req.prereqs.is_empty() {
        "(none)".to_string()
    } else {
        req.prereqs.join(", ")
    };
    format!(
        "Implement task '{id}': {intent}\n\
Files this task is expected to touch: {files}\n\
Prerequisite tasks already completed: {prereqs}\n\
Apply the change through the shuttle edit ladder (do not free-hand edits).",
        id = req.id,
        intent = req.intent,
    )
}

/// Parse an `intent/v1` DAG, turning a JSON error into a NavError (abort — never
/// silently proceed on a malformed contract).
fn parse_intent(raw: &str) -> Result<IntentDag, NavError> {
    serde_json::from_str::<IntentDag>(raw)
        .map_err(|e| io_err(&format!("intent plan is not valid intent/v1 JSON: {e}")))
}

fn io_err(msg: &str) -> NavError {
    NavError::Io(std::io::Error::other(msg.to_string()))
}

/// `shuttle goal` entrypoint. Exactly one of `goal` / `plan` must be set (clap
/// enforces mutual exclusion + required-ness upstream via `required = true`
/// on the group).
#[allow(clippy::too_many_arguments)]
pub fn run_goal(
    goal: Option<String>,
    plan: Option<String>,
    root: String,
    model: Option<String>,
    thinking: Option<String>,
    provider: Option<String>,
    base: Option<String>,
    head: Option<String>,
    max_depth: usize,
    dry_run: bool,
    parallel: bool,
    advice_out: Option<String>,
    advice_version: crate::AdviceVersion,
    symbol_footprint: Option<String>,
    observe: Option<String>,
    resume: Option<String>,
    telemetry: bool,
    reconcile: bool,
    diff: Option<String>,
    ledger: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let selected_advice_version = match (advice_version, symbol_footprint.as_deref()) {
        (crate::AdviceVersion::Auto, Some(_)) => crate::AdviceVersion::V2,
        (crate::AdviceVersion::Auto, None) => crate::AdviceVersion::V1,
        (crate::AdviceVersion::V1, Some(_)) => {
            anyhow::bail!("--symbol-footprint requires --advice-version v2")
        }
        (crate::AdviceVersion::V2, None) => {
            anyhow::bail!("--advice-version v2 requires --symbol-footprint")
        }
        (selected, _) => selected,
    };
    // G8/P3b reverse-reconcile short-circuit: with `--reconcile` we do NOT drive
    // the goal at all — we read the intent DAG (`--plan`) and a landed unified
    // diff (`--diff`), then mechanically re-derive per task satisfied / violated
    // / no-trace and list any `unattributed` diff path (never hard-attributed,
    // §4.3). shuttle owns no land权 (#11): the diff is an INPUT handed in. clap
    // already guarantees `--plan` and `--diff` are present with `--reconcile`.
    if reconcile {
        let plan_path = plan.expect("clap: --reconcile requires --plan");
        let diff_path = diff.expect("clap: --reconcile requires --diff");
        let outcome = shuttle_nav::reconcile::reconcile_files(&plan_path, &diff_path)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&outcome)?);
        } else {
            print_reconcile_outcome(&outcome);
        }
        // Exit 0 even on violation/unattributed: the state/reason_code fields are
        // the machine contract the caller routes on, matching goal/verify/split.
        return Ok(());
    }

    let advice_snapshot = if advice_out.is_some() {
        Some(GitTreeSnapshot::materialize(&root)?)
    } else {
        None
    };
    let analysis_root = advice_snapshot
        .as_ref()
        .map(|snapshot| snapshot.path.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.clone());
    let opts = QueryOptions::new(&analysis_root);

    // P2 (G8): where the observed footprint comes from. An explicit
    // `--observe <map.json>` (launcher/external/test) wins; else, in the live path,
    // shuttle reads the git working-tree delta per task; the hermetic dry-run path
    // observes nothing unless a map was supplied.
    let observe_mode = match &observe {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| io_err(&format!("read --observe map {path}: {e}")))?;
            let map: HashMap<String, Vec<String>> = serde_json::from_str(&raw).map_err(|e| {
                io_err(&format!(
                    "--observe map is not a JSON object of id -> [paths]: {e}"
                ))
            })?;
            ObserveMode::Injected(map)
        }
        None if dry_run || advice_out.is_some() => ObserveMode::None,
        None => ObserveMode::Git { root: root.clone() },
    };

    // P2 (G8) `--resume`: a persisted `GoalCheckpoint` JSON from a prior
    // (escalated or interrupted) session. Its completed tasks are skipped and
    // the re-plan tally continues (anti-thrash across resumes). shuttle owns no
    // checkpoint store (#11): the caller persists it and hands it back.
    let resume_cp = match &resume {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| io_err(&format!("read --resume checkpoint {path}: {e}")))?;
            let cp: GoalCheckpoint = serde_json::from_str(&raw)
                .map_err(|e| io_err(&format!("--resume file is not a GoalCheckpoint JSON: {e}")))?;
            Some(cp)
        }
        None => None,
    };

    let plan_source = match (goal, plan) {
        (_, Some(path)) => PlanSource::File(PathBuf::from(path)),
        (Some(g), None) => PlanSource::Goal {
            goal: g,
            model: model.clone(),
            thinking: thinking.clone(),
            provider: provider.clone(),
            root: analysis_root.clone(),
        },
        (None, None) => {
            anyhow::bail!("shuttle goal needs either --goal \"<text>\" or --plan <intent.json>")
        }
    };

    // The execute seam: dry-run (hermetic, zero model) or a real per-task pi.
    let exec = if dry_run || advice_out.is_some() {
        ExecMode::DryRun
    } else {
        ExecMode::Live {
            model,
            thinking,
            provider,
            root: analysis_root.clone(),
        }
    };

    let mut runner = CliGoalRunner {
        plan_source,
        opts,
        exec,
        base,
        head,
        max_depth,
        observe: observe_mode,
        seen: BTreeSet::new(),
        last_adjudicated_intent: None,
        last_base_oid: None,
        fixed_base_oid: advice_snapshot
            .as_ref()
            .map(|snapshot| snapshot.base_oid.clone()),
        frozen_intent_artifact: None,
        frozen_planning_revision: None,
    };

    // `drive_goal`'s first arg is only the goal *label* echoed into the result
    // when the DAG carries none; the actual decomposition source is the runner.
    let goal_label = match &runner.plan_source {
        PlanSource::Goal { goal, .. } => goal.clone(),
        PlanSource::File(_) => String::new(),
    };

    // When `--parallel` is set, compose the two provable concurrency oracles
    // (design §2.1): the file-overlap oracle first, then the symbol-coupling
    // oracle. `EitherOracle` is a fail-safe union — either serializing wins, so
    // the schedule only calls a batch concurrent when BOTH prove it safe (§2.3).
    // When resuming, the fully-parameterized entry threads the checkpoint in;
    // otherwise the plain wrappers keep the pre-P2 call sites byte-for-byte.
    let result = if resume_cp.is_some() {
        if parallel {
            let oracle = EitherOracle {
                first: FileOverlapOracle,
                second: SymbolCouplingOracle::new(&analysis_root),
            };
            drive_goal_resumable(&goal_label, &mut runner, Some(&oracle), resume_cp)?
        } else {
            drive_goal_resumable(&goal_label, &mut runner, None, resume_cp)?
        }
    } else if parallel {
        let oracle = EitherOracle {
            first: FileOverlapOracle,
            second: SymbolCouplingOracle::new(&analysis_root),
        };
        drive_goal_parallel(&goal_label, &mut runner, &oracle)?
    } else {
        drive_goal(&goal_label, &mut runner)?
    };

    // T5: one GoalTelemetry JSON line to stderr (feeds the next round of
    // decomposition); stdout stays the machine result. Pure projection.
    if telemetry {
        eprintln!("{}", serde_json::to_string(&result.telemetry)?);
    }

    // G9·D2: derive a replayable provenance ledger off the SAME adjudicated
    // split envelope embedded in the result (never a re-derivation). The
    // snapshot oid anchors every digest to the exact tree the evidence was read
    // at; a shallow/non-git tree yields an empty oid (honest, never fabricated).
    // Only the digest seed lands — the full evidence text stays out of the file.
    if let Some(path) = ledger.as_deref() {
        if let TypedDetail::Split(detail) = &result.split.detail {
            let snapshot_oid = shuttle_symbols::index::git_head_oid(&root).unwrap_or_default();
            let led = shuttle_protocol::ProvenanceLedger::from_split(detail, snapshot_oid);
            std::fs::write(path, serde_json::to_string_pretty(&led)?)?;
        }
    }

    // Export advice from the SAME final GoalResult that stdout reports. The
    // runner's provenance fields track the most recent adjudication, including
    // any drift-triggered re-plan. Missing schedule/provenance/base is a hard
    // failure: an explicit output request must never fabricate a binding.
    if let Some(path) = advice_out.as_deref() {
        let schedule = result.schedule.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "--advice-out requested, but the final goal result has no parallel schedule"
            )
        })?;
        let detail = match &result.split.detail {
            TypedDetail::Split(detail) => detail,
            _ => anyhow::bail!(
                "--advice-out requested, but the final goal result has no corrected split"
            ),
        };
        let intent = runner.last_adjudicated_intent.as_ref().ok_or_else(|| {
            anyhow::anyhow!("--advice-out requested without adjudicated intent provenance")
        })?;
        let base_oid = runner
            .last_base_oid
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--advice-out requested without base-OID provenance"))?;
        match selected_advice_version {
            crate::AdviceVersion::Auto => {
                unreachable!("auto advice version is resolved before execution")
            }
            crate::AdviceVersion::V1 => {
                let (symbol_index_digest, reproduced_schedule) =
                    capture_parallel_basis(&analysis_root, detail)?;
                if &reproduced_schedule != schedule {
                    anyhow::bail!(
                        "parallel scheduling inputs changed before advice publication; retry"
                    );
                }
                let (source_intent_ref, source_intent_digest) = artifact_binding(intent)?;
                let advice = ParallelAdvice::from_schedule(
                    schedule,
                    source_intent_ref,
                    source_intent_digest,
                    ParallelAdviceGraphBasis {
                        base_oid: base_oid.clone(),
                        symbol_index_digest,
                        correction_digest: correction_digest(detail)?,
                        footprint_source: FootprintSource::PlannerGuess,
                    },
                )?;
                write_json_atomically(path, &advice, Some((&root, &base_oid)))?;
            }
            crate::AdviceVersion::V2 => {
                let footprint_path = symbol_footprint
                    .as_deref()
                    .expect("v2 argument validation requires a footprint");
                let source_intent = runner.frozen_intent_artifact.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("v2 advice requires a frozen --plan artifact")
                })?;
                let planning_revision = runner
                    .frozen_planning_revision
                    .filter(|value| *value > 0)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "v2 advice requires a positive planning_revision in the frozen intent plan"
                        )
                    })?;
                let advice = build_hybrid_advice(
                    intent,
                    source_intent,
                    planning_revision,
                    &analysis_root,
                    &base_oid,
                    footprint_path,
                )?;
                write_json_atomically(path, &advice, Some((&root, &base_oid)))?;
            }
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        print_goal_result(&result);
    }

    // Exit 0 even on escalation: the phase/escalated/review fields are the
    // machine contract the caller routes on, matching split/impact.
    Ok(())
}

fn write_json_atomically<T: serde::Serialize>(
    path: &str,
    value: &T,
    expected_head: Option<(&str, &str)>,
) -> anyhow::Result<()> {
    use std::io::Write as _;

    let target = std::path::Path::new(path);
    let parent = target.parent().filter(|path| !path.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid --advice-out path: {path}"))?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmp_name = format!(".{file_name}.{}.{}.tmp", std::process::id(), nonce);
    let tmp = parent
        .unwrap_or_else(|| std::path::Path::new("."))
        .join(tmp_name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    let write_result = (|| -> anyhow::Result<()> {
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        if let Some((root, expected)) = expected_head {
            publish_with_locked_git_ref(root, expected, &tmp, target)?;
        } else {
            std::fs::rename(&tmp, target)?;
        }
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_result
}

fn build_hybrid_advice(
    intent: &IntentDag,
    source_intent: &serde_json::Value,
    planning_revision: u64,
    analysis_root: &str,
    base_oid: &str,
    footprint_path: &str,
) -> anyhow::Result<shuttle_protocol::HybridParallelAdvice> {
    let footprint_bytes = std::fs::read(footprint_path)?;
    let footprint = shuttle_protocol::SymbolFootprint::parse(&footprint_bytes)?;
    let intent_ids: BTreeSet<_> = intent.tasks.iter().map(|task| task.id.as_str()).collect();
    let footprint_ids: BTreeSet<_> = footprint
        .tasks
        .iter()
        .map(|task| task.task_id.as_str())
        .collect();
    if intent_ids != footprint_ids {
        anyhow::bail!("symbol footprint task ids must exactly match the frozen intent plan");
    }
    if footprint.basis.repositories.len() != 1
        || footprint.basis.repositories[0].base_oid != base_oid
    {
        anyhow::bail!("symbol footprint must bind exactly the frozen repository base {base_oid}");
    }

    let evidence_state = if footprint.basis.truncated {
        HybridEvidenceState::Truncated
    } else {
        match footprint.basis.freshness {
            shuttle_protocol::SymbolIndexFreshness::Fresh => {
                if footprint.basis.coverage == shuttle_protocol::SymbolCoverage::Exact {
                    HybridEvidenceState::FreshExactUnique
                } else {
                    HybridEvidenceState::Partial
                }
            }
            shuttle_protocol::SymbolIndexFreshness::Stale => HybridEvidenceState::Stale,
            shuttle_protocol::SymbolIndexFreshness::Absent => HybridEvidenceState::Absent,
            shuttle_protocol::SymbolIndexFreshness::Unsupported => HybridEvidenceState::Unsupported,
            shuttle_protocol::SymbolIndexFreshness::Timeout => HybridEvidenceState::Timeout,
        }
    };
    let model_edges: Vec<_> = intent
        .tasks
        .iter()
        .flat_map(|task| {
            task.depends_on_guess
                .iter()
                .map(move |dependency| HybridTaskEdge {
                    from: dependency.clone(),
                    to: task.id.clone(),
                })
        })
        .collect();

    let task_symbols: Vec<_> = footprint
        .tasks
        .iter()
        .map(|task| {
            let (source, writes) = if !task.observed.observed_write_symbols.is_empty() {
                (
                    HybridFootprintSource::ObservedDiff,
                    task.observed.observed_write_symbols.as_slice(),
                )
            } else {
                let source = match task.planned.evidence_source {
                    shuttle_protocol::PlannerEvidenceSource::PlannerDeclaredValidated
                        if evidence_state == HybridEvidenceState::FreshExactUnique =>
                    {
                        HybridFootprintSource::PlannerDeclaredValidated
                    }
                    _ => HybridFootprintSource::PlannerGuess,
                };
                (source, task.planned.declared_write_symbols.as_slice())
            };
            (task, source, writes)
        })
        .collect();

    let table = shuttle_nav::scan_table(&QueryOptions::new(analysis_root))?;
    let graph = shuttle_symbols::graph::SymbolGraph::try_from_scan_root(&table, analysis_root)?;
    let repo_id = &footprint.basis.repositories[0].repo_id;
    for task in &footprint.tasks {
        for identity in task
            .planned
            .declared_read_symbols
            .iter()
            .chain(&task.planned.declared_write_symbols)
            .chain(&task.observed.observed_write_symbols)
        {
            let matching = table
                .symbols
                .iter()
                .filter_map(|symbol| {
                    canonical_symbol_identity(symbol, analysis_root, repo_id, base_oid)
                })
                .filter(|actual| actual == identity)
                .count();
            if matching != 1 {
                anyhow::bail!(
                    "symbol footprint identity {} is not an exact unique member of the frozen snapshot",
                    identity.symbol_id
                );
            }
        }
    }
    let mut mechanical_edges = Vec::new();
    for (consumer, _, consumer_writes) in &task_symbols {
        for read in &consumer.planned.declared_read_symbols {
            for (provider, _, provider_writes) in &task_symbols {
                if consumer.task_id == provider.task_id
                    || !provider_writes
                        .iter()
                        .any(|write| write.symbol_id == read.symbol_id)
                {
                    continue;
                }
                for owner in *consumer_writes {
                    let Some(kind) = graph.edge_kind(&owner.name, &read.name) else {
                        continue;
                    };
                    mechanical_edges.push(HybridMechanicalEvidence {
                        owner_task: consumer.task_id.clone(),
                        callee_task: provider.task_id.clone(),
                        kind: match kind {
                            shuttle_symbols::graph::EdgeKind::ImportDirected => {
                                HybridEvidenceKind::ImportDirected
                            }
                            shuttle_symbols::graph::EdgeKind::ContractDirected => {
                                HybridEvidenceKind::ContractDirected
                            }
                            shuttle_symbols::graph::EdgeKind::NameMatch => {
                                HybridEvidenceKind::NameMatch
                            }
                            shuttle_symbols::graph::EdgeKind::CoChange => {
                                HybridEvidenceKind::CoChange
                            }
                        },
                        state: evidence_state,
                        evidence_ref: format!("symbol:{}", read.symbol_id),
                    });
                }
            }
        }
    }
    let write_claims = task_symbols
        .iter()
        .flat_map(|(task, _, writes)| {
            writes.iter().map(move |symbol| HybridWriteClaim {
                task_id: task.task_id.clone(),
                symbol_id: symbol.symbol_id.as_str().to_string(),
            })
        })
        .collect();
    let mut evidence_issues = Vec::new();
    if evidence_state != HybridEvidenceState::FreshExactUnique {
        evidence_issues.push(HybridEvidenceIssue {
            task_ids: intent.tasks.iter().map(|task| task.id.clone()).collect(),
            state: evidence_state,
            evidence_ref: "symbol-footprint-basis".to_string(),
        });
    }
    for task in &footprint.tasks {
        if !task.planned.unresolved.is_empty() || !task.observed.unresolved.is_empty() {
            evidence_issues.push(HybridEvidenceIssue {
                task_ids: vec![task.task_id.clone()],
                state: HybridEvidenceState::Ambiguous,
                evidence_ref: "symbol-footprint-unresolved".to_string(),
            });
        }
        let ambiguous_identity = task
            .planned
            .declared_read_symbols
            .iter()
            .chain(&task.planned.declared_write_symbols)
            .chain(&task.observed.observed_write_symbols)
            .any(|identity| {
                table
                    .symbols
                    .iter()
                    .filter(|symbol| symbol.name == identity.name)
                    .count()
                    != 1
            });
        if ambiguous_identity {
            evidence_issues.push(HybridEvidenceIssue {
                task_ids: vec![task.task_id.clone()],
                state: HybridEvidenceState::Ambiguous,
                evidence_ref: "symbol-name-ambiguous-in-nav-graph".to_string(),
            });
        }
    }
    let dag = merge_hybrid_dag(&HybridMergeInput {
        task_ids: intent.tasks.iter().map(|task| task.id.clone()).collect(),
        model_edges,
        mechanical_edges,
        write_claims,
        evidence_issues,
    });
    let footprints: Vec<_> = task_symbols
        .iter()
        .map(|(task, source, _)| HybridTaskFootprint {
            task_id: task.task_id.clone(),
            source: *source,
            state: evidence_state,
            write_paths: intent
                .tasks
                .iter()
                .find(|candidate| candidate.id == task.task_id)
                .map(|candidate| candidate.touched_files_guess.clone())
                .unwrap_or_default(),
        })
        .collect();

    let (source_intent_ref, source_intent_digest) = artifact_binding(source_intent)?;
    let footprint_digest = footprint.canonical_digest()?;
    let nav_advice = NavHybridParallelAdvice::from_hybrid_dag(
        &dag,
        &footprints,
        HybridPlanningBasis {
            planning_ref: source_intent_ref.clone(),
            planning_revision: planning_revision.to_string(),
            planning_structure_digest: canonical_digest(source_intent)?,
        },
        HybridArtifactBinding {
            artifact_ref: source_intent_ref,
            digest: source_intent_digest,
        },
        footprint
            .basis
            .repositories
            .iter()
            .map(|repository| HybridRepositoryBase {
                repo_id: repository.repo_id.clone(),
                base_oid: repository.base_oid.clone(),
            })
            .collect(),
        HybridArtifactBinding {
            artifact_ref: shuttle_protocol::HybridArtifactBinding::content_addressed(
                footprint_digest.clone(),
            )
            .artifact_ref,
            digest: footprint_digest,
        },
        footprint
            .basis
            .repositories
            .iter()
            .map(|repository| HybridIndexBinding {
                repo_id: repository.repo_id.clone(),
                index_digest: footprint.basis.symbol_index_digest.clone(),
            })
            .collect(),
    )?;
    let wire = serde_json::to_vec(&nav_advice)?;
    Ok(shuttle_protocol::HybridParallelAdvice::parse(&wire)?)
}

fn canonical_symbol_identity(
    symbol: &shuttle_symbols::Symbol,
    root: &str,
    repo_id: &str,
    base_oid: &str,
) -> Option<shuttle_protocol::SymbolIdentity> {
    let raw_path = std::path::Path::new(&symbol.path);
    let normalized_path = if raw_path.is_absolute() {
        raw_path
            .strip_prefix(std::path::Path::new(root))
            .ok()?
            .to_string_lossy()
            .replace('\\', "/")
    } else {
        symbol.path.replace('\\', "/")
    };
    let normalized_path = normalized_path
        .strip_prefix("./")
        .unwrap_or(&normalized_path)
        .to_string();
    let language = shuttle_symbols::lang_for_path(&normalized_path)?;
    let kind = serde_json::to_value(&symbol.kind)
        .ok()?
        .as_str()?
        .to_string();
    shuttle_protocol::SymbolIdentity::from_signature_bytes(
        repo_id,
        base_oid,
        language,
        normalized_path,
        symbol.name.clone(),
        kind,
        symbol.scope.clone(),
        u64::try_from(symbol.line).ok()?,
        symbol.signature.as_deref().unwrap_or_default().as_bytes(),
    )
    .ok()
}

fn publish_with_locked_git_ref(
    root: &str,
    expected_oid: &str,
    tmp: &std::path::Path,
    target: &std::path::Path,
) -> anyhow::Result<()> {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::process::{Command, Stdio};

    // Resolve once so every command participating in this ref transaction uses
    // the same verified executable while preserving the established argv/stdin
    // protocol verbatim.
    let git = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git)?;
    let symbolic = Command::new(&git)
        .args(["-C", root, "symbolic-ref", "-q", "HEAD"])
        .output()?;
    let reference = if symbolic.status.success() {
        String::from_utf8(symbolic.stdout)?.trim().to_string()
    } else {
        "HEAD".to_string()
    };
    let mut child = Command::new(&git)
        .args(["-C", root, "update-ref", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped update-ref stdin");
    let stdout = child.stdout.take().expect("piped update-ref stdout");
    let mut reader = BufReader::new(stdout);
    stdin.write_all(b"start\n")?;
    stdin.flush()?;
    let mut response = String::new();
    reader.read_line(&mut response)?;
    if response.trim() != "start: ok" {
        anyhow::bail!("could not start git ref transaction: {}", response.trim());
    }
    stdin.write_all(format!("verify {reference} {expected_oid}\nprepare\n").as_bytes())?;
    stdin.flush()?;
    response.clear();
    reader.read_line(&mut response)?;
    if response.trim() != "prepare: ok" {
        let _ = stdin.write_all(b"abort\n");
        let _ = stdin.flush();
        let output = child.wait_with_output()?;
        anyhow::bail!(
            "git HEAD changed before advice publication: {}{}",
            response.trim(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let head_target = Command::new(&git)
        .args(["-C", root, "rev-parse", "HEAD"])
        .output()?;
    let head_oid = String::from_utf8(head_target.stdout)?.trim().to_string();
    let symbolic_after = Command::new(&git)
        .args(["-C", root, "symbolic-ref", "-q", "HEAD"])
        .output()?;
    let reference_after = if symbolic_after.status.success() {
        String::from_utf8(symbolic_after.stdout)?.trim().to_string()
    } else {
        "HEAD".to_string()
    };
    let rename_result = if head_oid != expected_oid || reference_after != reference {
        Err(std::io::Error::other(
            "HEAD identity changed before advice publication",
        ))
    } else {
        std::fs::rename(tmp, target)
    };
    stdin.write_all(b"abort\n")?;
    stdin.flush()?;
    drop(stdin);
    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!(
            "could not release git ref publication lock: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    rename_result?;
    Ok(())
}

fn capture_parallel_basis(
    root: &str,
    detail: &SplitDetail,
) -> anyhow::Result<(shuttle_protocol::anchor::Sha256Digest, ParallelSchedule)> {
    fn snapshot(root: &str, detail: &SplitDetail) -> anyhow::Result<serde_json::Value> {
        let root = std::fs::canonicalize(root)?;
        let mut paths = BTreeSet::new();
        for node in &detail.nodes {
            for raw in &node.touched_files {
                let relative = std::path::Path::new(raw);
                if relative.is_absolute()
                    || relative
                        .components()
                        .any(|part| part == std::path::Component::ParentDir)
                {
                    anyhow::bail!("parallel advice path escapes repository: {raw}");
                }
                paths.insert(relative.to_path_buf());
            }
        }
        Ok(serde_json::json!({
            "files": paths.into_iter().map(|relative| {
                let bytes = std::fs::read(root.join(&relative)).ok();
                serde_json::json!({
                    "path": relative.to_string_lossy().replace('\\', "/"),
                    "content_digest": bytes.map(|bytes| format!("sha256:{}", shuttle_protocol::sha256_hex(&bytes))),
                })
            }).collect::<Vec<_>>()
        }))
    }

    let before = snapshot(root, detail)?;
    let oracle = EitherOracle {
        first: FileOverlapOracle,
        second: SymbolCouplingOracle::new(root),
    };
    let reproduced = schedule_parallel_with(detail, &oracle).map_err(|cycle| {
        anyhow::anyhow!("parallel schedule became cyclic: {}", cycle.join(", "))
    })?;
    let after = snapshot(root, detail)?;
    if before != after {
        anyhow::bail!("parallel scheduling inputs changed before advice publication; retry");
    }
    let ctags_version = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Ctags)
        .ok()
        .and_then(|ctags| {
            std::process::Command::new(ctags)
                .arg("--version")
                .output()
                .ok()
        })
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string()
        })
        .unwrap_or_else(|| "unavailable".to_string());
    let basis = serde_json::json!({
        "algorithm": "shuttle-symbol-coupling/v1",
        "ctags_version": ctags_version,
        "snapshot": before,
        "schedule": reproduced,
    });
    Ok((canonical_digest(&basis)?, reproduced))
}

struct GitTreeSnapshot {
    path: PathBuf,
    base_oid: String,
}

impl GitTreeSnapshot {
    fn materialize(root: &str) -> anyhow::Result<Self> {
        use std::process::{Command, Stdio};
        let base_oid = shuttle_symbols::index::git_head_oid(root).ok_or_else(|| {
            anyhow::anyhow!("--advice-out requires a readable git HEAD for {root}")
        })?;
        let git = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git)?;
        let tar = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Tar)?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "shuttle-advice-tree-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&path)?;
        let mut archive = Command::new(git)
            .args(["-C", root, "archive", "--format=tar", &base_oid])
            .stdout(Stdio::piped())
            .spawn()?;
        let archive_stdout = archive.stdout.take().expect("piped git archive stdout");
        let extracted = Command::new(tar)
            .args(["-xf", "-", "-C"])
            .arg(&path)
            .stdin(archive_stdout)
            .status()?;
        let archive_status = archive.wait()?;
        if !archive_status.success() || !extracted.success() {
            let _ = std::fs::remove_dir_all(&path);
            anyhow::bail!("failed to materialize immutable Git tree for advice");
        }
        Ok(Self { path, base_oid })
    }
}

impl Drop for GitTreeSnapshot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Human-readable rendering of a [`GoalResult`] (JSON mode is the machine
/// contract; this is the eyeball view).
fn print_goal_result(r: &GoalResult) {
    println!(
        "phase: {}  goal: {}",
        match r.phase {
            shuttle_nav::goal::GoalPhase::Landed => "landed",
            shuttle_nav::goal::GoalPhase::Escalated => "escalated",
        },
        if r.goal.is_empty() {
            "(unnamed)"
        } else {
            &r.goal
        }
    );
    println!(
        "adjudication: {}  reason: {}  (agree {}, add {}, drop {}, review {})",
        r.split.state.as_str(),
        r.split.reason_code,
        r.feedback.agree,
        r.feedback.added_by_symbolgraph,
        r.feedback.dropped_redundant,
        r.feedback.review_surface,
    );

    if r.escalated {
        println!("REVIEW REQUIRED — the loop stopped before executing:");
        for m in &r.review {
            println!("  - {m}");
        }
    } else {
        if !r.order.is_empty() {
            println!("execution order: {}", r.order.join(" -> "));
        }
        for rec in &r.exec {
            println!(
                "  [{}] {}  (patches {}, {}/{} accepted)",
                rec.order_index, rec.id, rec.patches, rec.accepted, rec.proposed
            );
        }
        if let Some(rev) = &r.reverify {
            if let TypedDetail::Reverify(d) = &rev.detail {
                println!(
                    "land re-verify set: {} unit(s){}",
                    d.reverify_size,
                    if d.degraded { " (degraded)" } else { "" }
                );
            }
        }
    }

    let t = &r.telemetry;
    println!(
        "telemetry: wall {}ms  tokens {}  tool_calls {}  patches {}  accept_rate {:.2}  reverify_size {}",
        t.wall_ms, t.tokens, t.tool_calls, t.patches, t.accept_rate, t.reverify_size
    );
}

/// Human-readable rendering of a `--reconcile` [`ToolOutcome`] (JSON mode is the
/// machine contract; this is the eyeball view). A non-Reconcile detail (an early
/// Error re-stamped under `reconcile`) falls back to state + reason.
fn print_reconcile_outcome(o: &ToolOutcome) {
    println!("state: {}  reason: {}", o.state.as_str(), o.reason_code);
    if let Some(s) = &o.summary {
        println!("{s}");
    }
    if let TypedDetail::Reconcile(d) = &o.detail {
        println!(
            "goal: {}  changed_files: {}  satisfied: {}  violated: {}  no_trace: {}  unattributed: {}",
            d.goal,
            d.changed_files.len(),
            d.satisfied,
            d.violated,
            d.no_trace,
            d.unattributed.len()
        );
        for t in &d.tasks {
            let tag = match t.verdict {
                ReconcileVerdict::Satisfied => "OK  ",
                ReconcileVerdict::Violated => "VIOL",
                ReconcileVerdict::NoTrace => "NONE",
            };
            match t.verdict {
                ReconcileVerdict::Satisfied => {
                    println!("  {tag} {}  ({} file(s))", t.id, t.matched_files.len())
                }
                ReconcileVerdict::Violated => println!("  {tag} {}  {}", t.id, t.evidence),
                ReconcileVerdict::NoTrace => println!("  {tag} {}", t.id),
            }
        }
        for u in &d.unattributed {
            println!("  ????  {u}  (fits no task — attribute manually, never guessed)");
        }
    }
    if let Some(a) = &o.next_action {
        println!("next: {a}");
    }
}

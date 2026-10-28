//! `shuttle goal` — the GoalGraph outer loop (G3).
//!
//! # What this is
//! `shuttle run` faces a *single requirement* and supervises one pi planning loop.
//! `shuttle goal` faces a *goal* and drives the full §4 outer loop around it, with a
//! built-in mechanical review gate:
//!
//! ```text
//! goal ──▶ plan            pi(goal-mode) emits an intent DAG (guessed deps)   [seam: plan]
//!      ──▶ adjudicate      shuttle split --correct (T1+T3) → three-state GoalGraph [seam: adjudicate]
//!            ├─ review_surface > 0  → ESCALATE (stop at the human review gate)
//!            └─ review_surface == 0 → the DAG is mechanically settled
//!      ──▶ order           topological order over the *corrected* edges
//!      ──▶ execute         per task: pi plan → shuttle apply ladder                [seam: execute]
//!      ──▶ land            shuttle impact --base/--head (T2) minimal re-verify set  [seam: reverify]
//!      ──▶ telemetry       GoalTelemetry (T5) — one JSON line for feedback
//! ```
//!
//! # Why an outer loop and not "review everything" (§4.3)
//! The adjudicator ([`crate::split`]) turns the model's guessed dependency DAG
//! into a three-state GoalGraph: edges both sources agree on are auto-kept
//! (AGREE), dependencies the symbol graph *proves* the model missed are
//! auto-added with a `symbolgraph` source label (ADD), redundant author edges
//! are dropped (DROP), and only genuine disagreements (CONFLICT/UNKNOWN)
//! surface to a human. So the review surface is `O(disagreements)`, never
//! `O(all tasks)` — and this loop **refuses to auto-execute a DAG that still
//! carries any unresolved edge** (§7 「绝不瞎猜」, red line #20). shuttle never holds
//! land权 (#11): the land step is still just the T2 re-verify *set*, computed
//! for whoever actually lands.
//!
//! # The one model stays in pi (§9 fair A/B)
//! No LLM is embedded here. Planning is the injected [`GoalRunner::plan`] seam
//! (pi in goal-mode); execution is [`GoalRunner::execute`] (pi per task). The
//! mechanical correction ([`GoalRunner::adjudicate`]) and the land set
//! ([`GoalRunner::reverify`]) are zero-model deterministic. The loop itself is
//! pure: every decision — the review gate, the topological order, when to stop,
//! how the telemetry is stitched — lives in tested code here, and only the four
//! seams touch the outside world. This mirrors
//! [`shuttle_protocol::orchestrate`](shuttle_protocol::orchestrate) (efficiency bet #3):
//! a pure state machine with a single injected boundary.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use shuttle_protocol::{
    reason, EdgeVerdict, GoalTelemetry, OutcomeState, SplitDetail, SplitFeedback, ToolOutcome,
    TypedDetail,
};

use crate::parallel::{schedule_parallel_with, ConcurrencyOracle, ParallelSchedule};
use crate::split::{adjudicate_intent, IntentDag, IntentTask, INTENT_VERSION};

/// Tool name stamped into any goal-produced envelope / telemetry.
pub const TOOL_GOAL: &str = "goal";

/// Default bound on how many times one outer loop will re-plan the
/// remaining tasks in response to land drift before it escalates (§3.3
/// anti-thrash, mirroring [`shuttle_protocol::orchestrate`]'s `max_attempts`).
pub const MAX_REPLAN: usize = 3;

// ---------------------------------------------------------------------------
// Seams (the only things that touch the outside world).
// ---------------------------------------------------------------------------

/// What the outer loop asks the executor to run for one task, in dependency
/// order. The `prereqs` are the corrected prerequisite task ids (from AGREE +
/// ADD edges — DROP edges are redundant and excluded), so an executor can
/// assemble the exact upstream contract (`shuttle context-pack`) a leaf needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskExecRequest {
    /// Stable task id (matches an [`IntentDag`] task / GoalGraph node).
    pub id: String,
    /// The task's intent label.
    pub intent: String,
    /// Files the corrected GoalGraph attributes to this task.
    pub touched_files: Vec<String>,
    /// Corrected prerequisite task ids (AGREE + ADD edges into this task).
    pub prereqs: Vec<String>,
    /// 0-based position in the topological execution order.
    pub order_index: usize,
}

/// What an executor returns for one task. All counts fold into the run's
/// [`GoalTelemetry`]; `accepted`/`proposed` aggregate into `accept_rate`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOutput {
    /// Patches actually applied for this task.
    pub patches: usize,
    /// Edit-plan ops proposed (denominator of the acceptance rate).
    pub proposed: usize,
    /// Edit-plan ops accepted (numerator of the acceptance rate).
    pub accepted: usize,
    /// Model tokens the task's planner consumed.
    pub tokens: u64,
    /// Tool calls the task made.
    pub tool_calls: u64,
}

/// The one boundary the outer loop delegates to the outside world, split into
/// the four distinct touches the §4 loop needs. Real launchers implement all
/// four (spawn pi to plan / execute, scan the symbol graph to adjudicate, diff
/// git to land); tests implement a scripted runner. Every *decision* the loop
/// makes around these calls stays in [`drive_goal`], not in the impl.
pub trait GoalRunner {
    /// Decompose a goal into a candidate intent DAG (pi in goal-mode). The
    /// `_guess` fields are exactly guesses — the adjudicator is what proves
    /// them. Returning [`crate::NavError`] aborts the whole loop.
    fn plan(&mut self, goal: &str) -> Result<IntentDag, crate::NavError>;

    /// Adjudicate the intent DAG into a three-state GoalGraph (T1 + T3).
    /// Production wires this to [`adjudicate_intent`]; the default here does
    /// exactly that against `Self`-supplied [`crate::QueryOptions`] via
    /// [`GoalRunner::query_options`].
    fn adjudicate(&mut self, intent: &IntentDag) -> Result<ToolOutcome, crate::NavError> {
        adjudicate_intent(intent, self.query_options())
    }

    /// The scan options the default [`GoalRunner::adjudicate`] runs against.
    fn query_options(&self) -> &crate::QueryOptions;

    /// Execute one task (pi plan → shuttle apply). Called once per task in
    /// topological order, only on the auto-execute (Green) path.
    fn execute(&mut self, req: &TaskExecRequest) -> Result<ExecOutput, crate::NavError>;

    /// Compute the land-time minimal re-verify set (T2 `shuttle impact
    /// --base/--head`). `None` means the loop did not reach a land (e.g. the
    /// caller passed no base/head), which is distinct from "landed, empty set".
    fn reverify(&mut self) -> Result<Option<ToolOutcome>, crate::NavError>;

    /// P2 (G8) drift seam. After a task executes, report the footprint the
    /// land step *mechanically observed* it touched — which may escape the
    /// files the GoalGraph attributed to it. `None` (the default) means the
    /// launcher makes no per-task observation, so the loop behaves exactly as
    /// pre-P2 (single pass, no re-planning). `Some(files)` that stray outside
    /// the task's attributed set is *drift*: the loop folds it back in and
    /// re-adjudicates (§3.2). This is a zero-model, mechanical touch — never a
    /// model call, never a guess.
    fn observe(
        &mut self,
        _req: &TaskExecRequest,
        _out: &ExecOutput,
    ) -> Result<Option<Vec<String>>, crate::NavError> {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Result types.
// ---------------------------------------------------------------------------

/// How far the outer loop got. `Escalated` is a *deliberate stop* at the review
/// gate (unresolved edges / a proven cycle), not a failure — it is the whole
/// point of the GoalGraph (§4.3-1). Everything else advances toward `Landed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalPhase {
    /// Stopped at the mechanical review gate — conflicts/unknowns or a proven
    /// dependency cycle need a human before any task runs.
    Escalated,
    /// All tasks executed and the land-time re-verify set was computed.
    Landed,
}

/// One executed task's record (for the transcript / JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskExecRecord {
    pub id: String,
    pub order_index: usize,
    pub patches: usize,
    pub proposed: usize,
    pub accepted: usize,
}

/// The full outcome of one `shuttle goal` outer loop.
#[derive(Debug, Clone, Serialize)]
pub struct GoalResult {
    /// The goal as decomposed (echoed from the adjudicated GoalGraph).
    pub goal: String,
    /// How far the loop got.
    pub phase: GoalPhase,
    /// True when the loop stopped at the review gate (`phase == Escalated`).
    pub escalated: bool,
    /// The mechanical adjudication envelope (the T1 split result verbatim).
    pub split: ToolOutcome,
    /// The per-run split feedback distilled from `split` (T5 projection).
    pub feedback: SplitFeedback,
    /// The topological execution order (empty when escalated).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<String>,
    /// Per-task execution records (empty when escalated).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exec: Vec<TaskExecRecord>,
    /// The human-review surface: the edges that must be adjudicated before the
    /// DAG can run (empty when the GoalGraph settled mechanically).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub review: Vec<String>,
    /// The land-time re-verify set (T2), when the loop reached a land.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reverify: Option<ToolOutcome>,
    /// The end-to-end telemetry (T5) — one JSON line the caller emits to feed
    /// the next round of decomposition.
    pub telemetry: GoalTelemetry,
    /// P1 (G8): the concurrency-batch schedule, present only on the
    /// `--parallel` path. `None` means the loop ran the plain topological
    /// order (no parallel analysis was requested). The batches are a scheduling
    /// *hint* — shuttle never owns the right to actually launch them (#11).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ParallelSchedule>,
    /// P2 (G8): the re-plan / resume checkpoint. Present when the loop
    /// re-planned or escalated *mid-execution* (so a `--resume` can skip the
    /// finished tasks), and `None` on the plain single-pass landed path where
    /// no checkpoint is needed. The completed list is authoritative — shuttle only
    /// ever records tasks it actually drove, never guesses progress (#11 / §7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<GoalCheckpoint>,
}

/// P2 (G8): a mid-execution checkpoint — the tasks the loop has already
/// driven (in execution order) and how many times it re-planned. A
/// `--resume` replays only the *un*finished tail, so already-landed work is
/// never redone. shuttle records only what it actually executed (§7 never-guess).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalCheckpoint {
    /// Task ids already executed, in the order they ran.
    pub completed: Vec<String>,
    /// How many times the loop re-planned the remaining tasks after drift.
    pub replans: usize,
}

impl GoalResult {
    /// Whether the loop landed (executed every task and computed the re-verify
    /// set) rather than stopping at the review gate.
    pub fn is_landed(&self) -> bool {
        self.phase == GoalPhase::Landed
    }
}

// ---------------------------------------------------------------------------
// Pure helpers.
// ---------------------------------------------------------------------------

/// The corrected prerequisites of `id`: the `from` of every AGREE/ADD edge that
/// points into `id`. DROP edges are redundant (already implied transitively) so
/// they are excluded; CONFLICT/UNKNOWN cannot reach here (Green path only).
fn prereqs_of(detail: &SplitDetail, id: &str) -> Vec<String> {
    let mut out: Vec<String> = detail
        .edges
        .iter()
        .filter(|e| e.to == id && matches!(e.verdict, EdgeVerdict::Agree | EdgeVerdict::Add))
        .map(|e| e.from.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Kahn topological order over the *corrected* edge set (AGREE + ADD only,
/// `from` = prerequisite → `to` = dependent). Deterministic: ties are broken by
/// the smallest task id. Returns `Err(cycle_nodes)` if the proven edges form a
/// cycle — the loop then escalates rather than guess an order (§7).
fn topo_order(detail: &SplitDetail) -> Result<Vec<String>, Vec<String>> {
    let mut indeg: BTreeMap<String, usize> = BTreeMap::new();
    for n in &detail.nodes {
        indeg.entry(n.id.clone()).or_insert(0);
    }
    let mut adj: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for e in &detail.edges {
        if !matches!(e.verdict, EdgeVerdict::Agree | EdgeVerdict::Add) {
            continue;
        }
        indeg.entry(e.from.clone()).or_insert(0);
        // Only count an edge once toward in-degree (parallel AGREE/ADD between
        // the same pair must not double-count).
        if adj.entry(e.from.clone()).or_default().insert(e.to.clone()) {
            *indeg.entry(e.to.clone()).or_insert(0) += 1;
        }
    }

    let mut zero: BTreeSet<String> = indeg
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(k, _)| k.clone())
        .collect();
    let mut order: Vec<String> = Vec::new();
    while let Some(n) = zero.iter().next().cloned() {
        zero.remove(&n);
        order.push(n.clone());
        if let Some(outs) = adj.get(&n) {
            for m in outs {
                let d = indeg.get_mut(m).expect("edge endpoint is a known node");
                *d -= 1;
                if *d == 0 {
                    zero.insert(m.clone());
                }
            }
        }
    }

    if order.len() == indeg.len() {
        Ok(order)
    } else {
        let done: BTreeSet<&String> = order.iter().collect();
        let cycle: Vec<String> = indeg
            .keys()
            .filter(|k| !done.contains(k))
            .cloned()
            .collect();
        Err(cycle)
    }
}

// ---------------------------------------------------------------------------
// The outer loop.
// ---------------------------------------------------------------------------

/// Drive the full GoalGraph outer loop for `goal`, delegating the four
/// outside-world touches to `runner`.
///
/// Pure except for the `runner` seams and a wall clock. Terminates in at most
/// `N` executor calls, where `N` is the number of tasks in the adjudicated DAG
/// (a finite DAG, executed once each). A [`crate::NavError`] from any seam
/// aborts the loop; every other path — including a review escalation — is an
/// `Ok` [`GoalResult`].
pub fn drive_goal<R: GoalRunner>(
    goal: &str,
    runner: &mut R,
) -> Result<GoalResult, crate::NavError> {
    drive_goal_scheduled(goal, runner, None, None)
}

/// Same corrector→runtime loop as [`drive_goal`], but with a concurrency
/// oracle that partitions the topo order into provably-independent batches
/// (G8/P1). The oracle only ever *serializes* — an unprovable independence
/// degrades to serial (§7), never to an optimistic parallel guess. shuttle owns no
/// launcher and no land权 (#11); the batches are a scheduling *hint* attached
/// to the result, and execution still walks a valid (batch-major) topo order.
pub fn drive_goal_parallel<R: GoalRunner>(
    goal: &str,
    runner: &mut R,
    oracle: &dyn ConcurrencyOracle,
) -> Result<GoalResult, crate::NavError> {
    drive_goal_scheduled(goal, runner, Some(oracle), None)
}

/// The fully-parameterized entry (G8/P2b `--resume`): run the corrector→runtime
/// loop with an optional concurrency oracle *and* an optional [`GoalCheckpoint`]
/// to resume from. When `resume` is `Some`, its `completed` tasks are treated as
/// already landed — the walk skips them (never redoes a finished task) and the
/// re-plan budget continues from where the prior session left off (the cap is
/// anti-thrash across resumes, §3.3). `plan` + `adjudicate` still re-run (they
/// are deterministic over the same tree), so the DAG the resume walks is the
/// same one the interrupted session had. shuttle owns no checkpoint store (#11): the
/// caller persists the checkpoint from a prior result and hands it back here.
pub fn drive_goal_resumable<R: GoalRunner>(
    goal: &str,
    runner: &mut R,
    oracle: Option<&dyn ConcurrencyOracle>,
    resume: Option<GoalCheckpoint>,
) -> Result<GoalResult, crate::NavError> {
    drive_goal_scheduled(goal, runner, oracle, resume)
}

fn drive_goal_scheduled<R: GoalRunner>(
    goal: &str,
    runner: &mut R,
    oracle: Option<&dyn ConcurrencyOracle>,
    resume: Option<GoalCheckpoint>,
) -> Result<GoalResult, crate::NavError> {
    let started = Instant::now();

    // 1. plan — pi(goal-mode) guesses the intent DAG.
    let intent = runner.plan(goal)?;

    // 2. adjudicate — the mechanical three-state review gate (T1 + T3).
    let split = runner.adjudicate(&intent)?;
    let feedback = SplitFeedback::from_outcome(&split).unwrap_or_default();
    let goal_label = match &split.detail {
        TypedDetail::Split(d) if !d.goal.is_empty() => d.goal.clone(),
        _ => intent.goal.clone(),
    };

    // 2a. review gate. A non-Green adjudication carries unresolved
    // CONFLICT/UNKNOWN edges — shuttle never auto-executes over them (§7, #20). Stop
    // and surface exactly those edges (O(disagreements)).
    if split.state != OutcomeState::Green {
        let review = split.whats_missing.clone();
        let wall_ms = started.elapsed().as_millis() as u64;
        let telemetry = GoalTelemetry::new(&feedback, wall_ms, 0, 0, 0, 0, 0, 0);
        return Ok(GoalResult {
            goal: goal_label,
            phase: GoalPhase::Escalated,
            escalated: true,
            split,
            feedback,
            order: Vec::new(),
            exec: Vec::new(),
            review,
            reverify: None,
            telemetry,
            schedule: None,
            checkpoint: None,
        });
    }

    // Green ⇒ the adjudication is a Split detail (its lowering guarantees it).
    let detail = match &split.detail {
        TypedDetail::Split(d) => d.clone(),
        _ => {
            // An internally inconsistent envelope (Green but not a split) is a
            // bug in the adjudicator, never something to guess around.
            return Ok(escalate_internal(goal_label, split, feedback, started));
        }
    };

    // 3. topological order over the corrected edges. A proven cycle escalates
    // rather than picks an arbitrary order (§7). The order itself is recomputed
    // per pass by `plan_order`; here we only guard the up-front cycle case.
    if let Err(cycle) = topo_order(&detail) {
        let mut review: Vec<String> = vec![format!(
            "proven dependency cycle among tasks: {}",
            cycle.join(", ")
        )];
        review.sort();
        let wall_ms = started.elapsed().as_millis() as u64;
        let telemetry = GoalTelemetry::new(&feedback, wall_ms, 0, 0, 0, 0, 0, 0);
        return Ok(GoalResult {
            goal: goal_label,
            phase: GoalPhase::Escalated,
            escalated: true,
            split,
            feedback,
            order: Vec::new(),
            exec: Vec::new(),
            review,
            reverify: None,
            telemetry,
            schedule: None,
            checkpoint: None,
        });
    }

    // 3a. concurrency schedule (G8/P1). When an oracle is present, partition
    // the topo order into antichain batches of provably file/symbol-disjoint
    // tasks. The scheduler is defensive: any pair it cannot *prove* independent
    // is serialized (§7). If it discovers a cycle the topo pass did not, that
    // is an internal inconsistency — escalate rather than pick an order.
    let schedule = match oracle {
        Some(o) => match schedule_parallel_with(&detail, o) {
            Ok(sched) => Some(sched),
            Err(cycle) => {
                let mut review: Vec<String> = vec![format!(
                    "parallel scheduler found a cycle the topo pass did not: {}",
                    cycle.join(", ")
                )];
                review.sort();
                let wall_ms = started.elapsed().as_millis() as u64;
                let telemetry = GoalTelemetry::new(&feedback, wall_ms, 0, 0, 0, 0, 0, 0);
                return Ok(GoalResult {
                    goal: goal_label,
                    phase: GoalPhase::Escalated,
                    escalated: true,
                    split,
                    feedback,
                    order: Vec::new(),
                    exec: Vec::new(),
                    review,
                    reverify: None,
                    telemetry,
                    schedule: None,
                    checkpoint: None,
                });
            }
        },
        None => None,
    };

    // 4. execute the tasks (pi plan → shuttle apply), with the P2 re-plan loop
    // wrapped around the walk. Each pass executes the not-yet-completed tasks
    // of the current (possibly re-planned) GoalGraph in dependency order; if a
    // task's *observed* footprint drifts outside what the GoalGraph attributed
    // to it, we fold the drift back in, re-adjudicate the remaining tasks, and
    // restart the walk (skipping everything already completed). The cap on
    // re-plans is anti-thrash (§3.3); an unexplainable drift or a re-plan that
    // no longer settles Green escalates with the checkpoint intact (§7).
    let mut cur_split = split;
    let mut cur_feedback = feedback;
    let mut cur_detail = detail;
    let mut cur_schedule = schedule;

    // On `--resume`, the prior session's landed prefix and re-plan tally are the
    // starting state: the walk skips everything already completed and the cap
    // continues from the prior count (§3.3 anti-thrash across resumes). A run
    // without a checkpoint starts clean, exactly as before.
    let (mut completed, mut replans): (Vec<String>, usize) = match resume {
        Some(cp) => (cp.completed, cp.replans),
        None => (Vec::new(), 0),
    };
    let resumed = !completed.is_empty() || replans > 0;
    let mut exec: Vec<TaskExecRecord> = Vec::new();
    let (mut patches, mut proposed, mut accepted, mut tokens, mut tool_calls) =
        (0usize, 0usize, 0usize, 0u64, 0u64);

    'replan: loop {
        // The execution order of the current plan (batch-major when scheduled,
        // else the plain topo order it was computed from).
        let cur_order = plan_order(&cur_detail, &cur_schedule);
        let node_by_id: BTreeMap<&str, &shuttle_protocol::SplitNode> = cur_detail
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), n))
            .collect();

        let mut re_planned = false;
        for id in &cur_order {
            if completed.iter().any(|c| c == id) {
                continue; // already driven on an earlier pass — never redo it.
            }
            let node = node_by_id.get(id.as_str());
            let req = TaskExecRequest {
                id: id.clone(),
                intent: node.map(|n| n.intent.clone()).unwrap_or_default(),
                touched_files: node.map(|n| n.touched_files.clone()).unwrap_or_default(),
                prereqs: prereqs_of(&cur_detail, id),
                order_index: completed.len(),
            };
            let out = runner.execute(&req)?;
            patches += out.patches;
            proposed += out.proposed;
            accepted += out.accepted;
            tokens += out.tokens;
            tool_calls += out.tool_calls;
            exec.push(TaskExecRecord {
                id: id.clone(),
                order_index: completed.len(),
                patches: out.patches,
                proposed: out.proposed,
                accepted: out.accepted,
            });
            completed.push(id.clone());

            // P2 drift check (§3.2). The launcher's mechanically-observed
            // footprint may escape the files the GoalGraph attributed to the
            // task. Files outside the attributed set are drift.
            let drift = match runner.observe(&req, &out)? {
                Some(observed) => footprint_drift(&req.touched_files, &observed),
                None => Vec::new(),
            };
            if drift.is_empty() {
                continue; // footprint matched expectation — no re-plan needed.
            }

            // Drift → re-plan the remaining tasks. First the anti-thrash cap.
            if replans >= MAX_REPLAN {
                let review = vec![format!(
                    "re-plan cap ({MAX_REPLAN}) hit: task `{id}` drifted onto {} \
                     after {replans} re-plans",
                    drift.join(", ")
                )];
                return Ok(escalate_mid_exec(
                    goal_label,
                    cur_split,
                    cur_feedback,
                    cur_order,
                    exec,
                    review,
                    completed,
                    replans,
                    tokens,
                    tool_calls,
                    patches,
                    proposed,
                    accepted,
                    started,
                ));
            }
            replans += 1;

            // Fold the observed drift into a fresh intent over the *same* task
            // universe and re-adjudicate. Re-adjudication re-scans reality
            // (the tasks already edited the tree), so any newly-provable
            // dependency shows up mechanically — never guessed.
            let working = intent_with_drift(&cur_detail, id, &drift);
            let new_split = runner.adjudicate(&working)?;
            let new_feedback = SplitFeedback::from_outcome(&new_split).unwrap_or_default();

            // A re-plan that no longer settles Green means the drift introduced
            // a genuine conflict/unknown — escalate, do not guess around it.
            if new_split.state != OutcomeState::Green {
                let review = new_split.whats_missing.clone();
                return Ok(escalate_mid_exec(
                    goal_label,
                    new_split,
                    new_feedback,
                    cur_order,
                    exec,
                    review,
                    completed,
                    replans,
                    tokens,
                    tool_calls,
                    patches,
                    proposed,
                    accepted,
                    started,
                ));
            }
            let new_detail = match &new_split.detail {
                TypedDetail::Split(d) => d.clone(),
                _ => {
                    return Ok(escalate_mid_exec(
                        goal_label,
                        new_split,
                        new_feedback,
                        cur_order,
                        exec,
                        vec![format!(
                            "re-plan produced a non-split {} envelope",
                            reason::INTERNAL
                        )],
                        completed,
                        replans,
                        tokens,
                        tool_calls,
                        patches,
                        proposed,
                        accepted,
                        started,
                    ));
                }
            };
            // The re-planned DAG must still admit a topological order and, when
            // scheduling, a valid batch partition — a cycle either introduces
            // escalates (never an arbitrary order).
            if let Err(cycle) = topo_order(&new_detail) {
                let review = vec![format!(
                    "re-plan introduced a dependency cycle among tasks: {}",
                    cycle.join(", ")
                )];
                return Ok(escalate_mid_exec(
                    goal_label,
                    new_split,
                    new_feedback,
                    cur_order,
                    exec,
                    review,
                    completed,
                    replans,
                    tokens,
                    tool_calls,
                    patches,
                    proposed,
                    accepted,
                    started,
                ));
            }
            let new_schedule = match oracle {
                Some(o) => match schedule_parallel_with(&new_detail, o) {
                    Ok(sched) => Some(sched),
                    Err(cycle) => {
                        let review = vec![format!(
                            "re-plan scheduler found a cycle: {}",
                            cycle.join(", ")
                        )];
                        return Ok(escalate_mid_exec(
                            goal_label,
                            new_split,
                            new_feedback,
                            cur_order,
                            exec,
                            review,
                            completed,
                            replans,
                            tokens,
                            tool_calls,
                            patches,
                            proposed,
                            accepted,
                            started,
                        ));
                    }
                },
                None => None,
            };

            cur_split = new_split;
            cur_feedback = new_feedback;
            cur_detail = new_detail;
            cur_schedule = new_schedule;
            re_planned = true;
            break; // restart the walk over the re-planned remaining tasks.
        }

        if !re_planned {
            break 'replan; // a full pass with no drift — every task is landed.
        }
    }

    // The actual executed sequence is a valid (batch-major) topological order.
    let order = completed.clone();

    // 5. land-time minimal re-verify set (T2). shuttle never lands; it reasons.
    let reverify = runner.reverify()?;
    let reverify_size = reverify
        .as_ref()
        .and_then(shuttle_protocol::reverify_size_of)
        .unwrap_or(0);

    // 6. stitch the end-to-end telemetry (T5).
    let wall_ms = started.elapsed().as_millis() as u64;
    let telemetry = GoalTelemetry::new(
        &cur_feedback,
        wall_ms,
        tokens,
        tool_calls,
        reverify_size,
        patches,
        proposed,
        accepted,
    );

    // A checkpoint is attached only when the loop actually re-planned mid-way
    // (so a --resume can skip the finished tasks); the plain single-pass path
    // needs none.
    let checkpoint = if replans > 0 || resumed {
        Some(GoalCheckpoint {
            completed: completed.clone(),
            replans,
        })
    } else {
        None
    };

    Ok(GoalResult {
        goal: goal_label,
        phase: GoalPhase::Landed,
        escalated: false,
        split: cur_split,
        feedback: cur_feedback,
        order,
        exec,
        review: Vec::new(),
        reverify,
        telemetry,
        schedule: cur_schedule,
        checkpoint,
    })
}

// ---------------------------------------------------------------------------
// P2 (G8) re-plan helpers — all pure, all deterministic, no I/O, no guessing.
// ---------------------------------------------------------------------------

/// The execution order for a (possibly re-planned) plan: the batch-major
/// flatten when a schedule is present, else the plain topological order over
/// the corrected edges. Both are valid topological orders (each batch only
/// holds tasks whose prereqs live in earlier batches), so the executed
/// sequence is dependency-correct either way.
fn plan_order(detail: &SplitDetail, schedule: &Option<ParallelSchedule>) -> Vec<String> {
    match schedule {
        Some(sched) => sched
            .batches
            .iter()
            .flat_map(|b| b.tasks.iter().cloned())
            .collect(),
        // topo_order only errs on a cycle, which the caller already ruled out
        // before we reach here; fall back to node id order if it somehow does.
        None => topo_order(detail).unwrap_or_else(|_| {
            let mut ids: Vec<String> = detail.nodes.iter().map(|n| n.id.clone()).collect();
            ids.sort();
            ids
        }),
    }
}

/// The drift: observed files that fall *outside* the set the GoalGraph
/// attributed to a task. Paths are normalized the same way the adjudicator
/// normalizes `touched_files_guess`, so the comparison is apples-to-apples.
/// Empty ⇒ the observed footprint stayed within expectation (no re-plan).
fn footprint_drift(attributed: &[String], observed: &[String]) -> Vec<String> {
    let expect: BTreeSet<String> = attributed.iter().map(|f| norm_path(f)).collect();
    let mut extra: Vec<String> = observed
        .iter()
        .map(|f| norm_path(f))
        .filter(|f| !expect.contains(f))
        .collect();
    extra.sort();
    extra.dedup();
    extra
}

/// Normalize a path the way the split adjudicator does (strip a leading
/// `./`), so drift comparison and the re-planned intent agree with it.
fn norm_path(f: &str) -> String {
    f.strip_prefix("./").unwrap_or(f).to_string()
}

/// Rebuild an [`IntentDag`] over the *current* GoalGraph, folding the observed
/// `drift` files into `drifted_id`'s `touched_files_guess`. Prerequisites are
/// carried over verbatim from the corrected AGREE/ADD edges (so the re-plan
/// keeps everything already proven); the fresh adjudication then re-scans
/// reality and lets any newly-provable dependency surface mechanically. The
/// intent this produces is a faithful *input* to re-adjudication — it invents
/// no dependency, only records the enlarged footprint.
fn intent_with_drift(detail: &SplitDetail, drifted_id: &str, drift: &[String]) -> IntentDag {
    let tasks: Vec<IntentTask> = detail
        .nodes
        .iter()
        .map(|n| {
            let mut files: Vec<String> = n.touched_files.clone();
            if n.id == drifted_id {
                for d in drift {
                    if !files.iter().any(|f| norm_path(f) == norm_path(d)) {
                        files.push(d.clone());
                    }
                }
            }
            IntentTask {
                id: n.id.clone(),
                intent: n.intent.clone(),
                touched_files_guess: files,
                depends_on_guess: prereqs_of(detail, &n.id),
                acceptance: None,
                invariants: None,
            }
        })
        .collect();
    IntentDag {
        goal: detail.goal.clone(),
        tasks,
        version: INTENT_VERSION.to_string(),
    }
}

/// Escalate *after* some tasks already executed (P2). Unlike the pre-execution
/// escalations, this carries the [`GoalCheckpoint`] of completed work so a
/// `--resume` never redoes a landed task, plus the runtime tallies gathered so
/// far. It is a deliberate stop at the review gate (§3.3), not a failure.
#[allow(clippy::too_many_arguments)]
fn escalate_mid_exec(
    goal_label: String,
    split: ToolOutcome,
    feedback: SplitFeedback,
    order: Vec<String>,
    exec: Vec<TaskExecRecord>,
    review: Vec<String>,
    completed: Vec<String>,
    replans: usize,
    tokens: u64,
    tool_calls: u64,
    patches: usize,
    proposed: usize,
    accepted: usize,
    started: Instant,
) -> GoalResult {
    let mut review = review;
    review.sort();
    let wall_ms = started.elapsed().as_millis() as u64;
    let telemetry = GoalTelemetry::new(
        &feedback, wall_ms, tokens, tool_calls, 0, patches, proposed, accepted,
    );
    GoalResult {
        goal: goal_label,
        phase: GoalPhase::Escalated,
        escalated: true,
        split,
        feedback,
        order,
        exec,
        review,
        reverify: None,
        telemetry,
        schedule: None,
        checkpoint: Some(GoalCheckpoint { completed, replans }),
    }
}

/// Escalate on an internally inconsistent (Green-but-not-Split) adjudication —
/// treated as a review escalation, never guessed around.
fn escalate_internal(
    goal_label: String,
    split: ToolOutcome,
    feedback: SplitFeedback,
    started: Instant,
) -> GoalResult {
    let wall_ms = started.elapsed().as_millis() as u64;
    let telemetry = GoalTelemetry::new(&feedback, wall_ms, 0, 0, 0, 0, 0, 0);
    GoalResult {
        goal: goal_label,
        phase: GoalPhase::Escalated,
        escalated: true,
        split,
        feedback,
        order: Vec::new(),
        exec: Vec::new(),
        review: vec![format!(
            "adjudicator returned a non-split {} envelope",
            reason::INTERNAL
        )],
        reverify: None,
        telemetry,
        schedule: None,
        checkpoint: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shuttle_protocol::{
        EdgeSource, EdgeVerdict, OutcomeConfidence, OutcomeState, ReverifyDetail, ReverifyUnit,
        SplitEdge, SplitNode, TypedDetail,
    };
    use std::collections::BTreeMap;

    use crate::split::{IntentDag, INTENT_VERSION};

    // --- builders ---------------------------------------------------------

    fn qopts() -> crate::QueryOptions {
        crate::QueryOptions::new(".")
    }

    fn node(id: &str) -> SplitNode {
        SplitNode {
            id: id.into(),
            intent: format!("intent-{id}"),
            touched_files: vec![format!("{id}.py")],
        }
    }

    fn edge(from: &str, to: &str, verdict: EdgeVerdict, source: EdgeSource) -> SplitEdge {
        SplitEdge {
            from: from.into(),
            to: to.into(),
            verdict,
            confidence: OutcomeConfidence::High,
            source,
            evidence: String::new(),
        }
    }

    /// A `SplitDetail` with the review tallies derived from the edges (exactly
    /// how `correct_split` fills them), so the fixture can never disagree with
    /// its own edge list.
    fn detail(nodes: &[&str], edges: Vec<SplitEdge>) -> SplitDetail {
        let mut review_required = Vec::new();
        for (i, e) in edges.iter().enumerate() {
            if e.verdict.needs_review() {
                review_required.push(i);
            }
        }
        let added = edges
            .iter()
            .filter(|e| e.verdict == EdgeVerdict::Add)
            .count();
        let dropped = edges
            .iter()
            .filter(|e| e.verdict == EdgeVerdict::Drop)
            .count();
        let review_surface = review_required.len();
        SplitDetail {
            goal: "G".into(),
            nodes: nodes.iter().map(|n| node(n)).collect(),
            edges,
            added,
            dropped,
            review_surface,
            review_required,
        }
    }

    fn green_split(detail: SplitDetail) -> ToolOutcome {
        ToolOutcome::green(TOOL_GOAL, TypedDetail::Split(detail))
    }

    fn blocked_review(detail: SplitDetail, missing: Vec<&str>) -> ToolOutcome {
        ToolOutcome::blocked(
            TOOL_GOAL,
            reason::NEEDS_REVIEW,
            "adjudicate the conflicting / unprovable edges",
            TypedDetail::Split(detail),
        )
        .with_missing(missing.into_iter().map(String::from).collect())
    }

    fn reverify_outcome(size: usize) -> ToolOutcome {
        let reverify: Vec<ReverifyUnit> = (0..size)
            .map(|i| ReverifyUnit {
                path: format!("u{i}.py"),
                reason: if i == 0 { "changed" } else { "reverse-dep" }.into(),
                confidence: OutcomeConfidence::High,
                distance: i,
            })
            .collect();
        ToolOutcome::green(
            "impact",
            TypedDetail::Reverify(ReverifyDetail {
                base_oid: "base".into(),
                head_oid: "head".into(),
                index_base_oid: "head".into(),
                changed_files: vec!["u0.py".into()],
                reverify,
                granularity: "file".into(),
                degraded: false,
                reverify_size: size,
            }),
        )
    }

    fn exec(
        patches: usize,
        proposed: usize,
        accepted: usize,
        tokens: u64,
        calls: u64,
    ) -> ExecOutput {
        ExecOutput {
            patches,
            proposed,
            accepted,
            tokens,
            tool_calls: calls,
        }
    }

    // --- the scripted runner ---------------------------------------------

    /// A `GoalRunner` whose four seams are all canned, so the outer loop can be
    /// exercised without ctags / a filesystem. It records every `execute`
    /// request in order for assertions.
    struct Script {
        opts: crate::QueryOptions,
        plan_err: bool,
        adj: ToolOutcome,
        /// Adjudications returned on *re-plan* passes (index 0 = the first
        /// re-adjudication, and so on). When exhausted, `adj` is reused. This
        /// lets a test script a drift that re-settles Green, or one that
        /// re-adjudicates to a blocked review, deterministically.
        replan_adj: Vec<ToolOutcome>,
        /// How many times `adjudicate` has been called (0 = the initial pass).
        adj_idx: usize,
        rev: Option<ToolOutcome>,
        execs: BTreeMap<String, ExecOutput>,
        default_exec: ExecOutput,
        exec_err_on: Option<String>,
        /// Per-task footprint the launcher "observed". Present ⇒ `observe`
        /// returns `Some(files)` for that task (any file outside its attributed
        /// set is drift); absent ⇒ `observe` returns `None` (no observation).
        observe_drift: BTreeMap<String, Vec<String>>,
        calls: Vec<TaskExecRequest>,
    }

    impl Script {
        fn new(adj: ToolOutcome) -> Self {
            Script {
                opts: qopts(),
                plan_err: false,
                adj,
                replan_adj: Vec::new(),
                adj_idx: 0,
                rev: Some(reverify_outcome(1)),
                execs: BTreeMap::new(),
                default_exec: ExecOutput::default(),
                exec_err_on: None,
                observe_drift: BTreeMap::new(),
                calls: Vec::new(),
            }
        }
    }

    fn io_err() -> crate::NavError {
        crate::NavError::Io(std::io::Error::other("scripted"))
    }

    impl GoalRunner for Script {
        fn plan(&mut self, goal: &str) -> Result<IntentDag, crate::NavError> {
            if self.plan_err {
                return Err(io_err());
            }
            Ok(IntentDag {
                goal: goal.to_string(),
                tasks: Vec::new(),
                version: INTENT_VERSION.into(),
            })
        }

        // Override the default (which would scan the filesystem) with the
        // canned adjudication. The first call is the initial pass; subsequent
        // calls are re-plans and consume `replan_adj` in order.
        fn adjudicate(&mut self, _intent: &IntentDag) -> Result<ToolOutcome, crate::NavError> {
            let out = if self.adj_idx == 0 {
                self.adj.clone()
            } else {
                self.replan_adj
                    .get(self.adj_idx - 1)
                    .cloned()
                    .unwrap_or_else(|| self.adj.clone())
            };
            self.adj_idx += 1;
            Ok(out)
        }

        fn query_options(&self) -> &crate::QueryOptions {
            &self.opts
        }

        fn execute(&mut self, req: &TaskExecRequest) -> Result<ExecOutput, crate::NavError> {
            self.calls.push(req.clone());
            if self.exec_err_on.as_deref() == Some(req.id.as_str()) {
                return Err(io_err());
            }
            Ok(self
                .execs
                .get(&req.id)
                .cloned()
                .unwrap_or_else(|| self.default_exec.clone()))
        }

        fn reverify(&mut self) -> Result<Option<ToolOutcome>, crate::NavError> {
            Ok(self.rev.clone())
        }

        fn observe(
            &mut self,
            req: &TaskExecRequest,
            _out: &ExecOutput,
        ) -> Result<Option<Vec<String>>, crate::NavError> {
            Ok(self.observe_drift.get(&req.id).cloned())
        }
    }

    // --- pure-helper tests -----------------------------------------------

    #[test]
    fn topo_order_is_deterministic_by_smallest_id() {
        // No edges: every node is a root, emitted in id order.
        let d = detail(&["c", "a", "b"], vec![]);
        assert_eq!(topo_order(&d).unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn topo_order_respects_agree_and_add_edges() {
        // base → mid → top chain, plus an independent ADD edge x → y.
        let d = detail(
            &["top", "mid", "base", "y", "x"],
            vec![
                edge("base", "mid", EdgeVerdict::Agree, EdgeSource::Both),
                edge("mid", "top", EdgeVerdict::Agree, EdgeSource::Both),
                edge("x", "y", EdgeVerdict::Add, EdgeSource::Symbolgraph),
            ],
        );
        let order = topo_order(&d).unwrap();
        let pos = |id: &str| order.iter().position(|x| x == id).unwrap();
        assert!(pos("base") < pos("mid"));
        assert!(pos("mid") < pos("top"));
        assert!(pos("x") < pos("y"));
    }

    #[test]
    fn topo_order_ignores_drop_conflict_unknown_edges() {
        // Only the DROP/CONFLICT/UNKNOWN edges exist — none constrain order, so
        // all three nodes are roots (id order).
        let d = detail(
            &["a", "b", "c"],
            vec![
                edge("a", "b", EdgeVerdict::Drop, EdgeSource::Intent),
                edge("b", "c", EdgeVerdict::Conflict, EdgeSource::Intent),
                edge("c", "a", EdgeVerdict::Unknown, EdgeSource::Intent),
            ],
        );
        // NB: these verdicts would escalate before topo in the loop; here we
        // test the pure helper in isolation.
        assert_eq!(topo_order(&d).unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn topo_order_dedups_parallel_edges_without_underflow() {
        // Two AGREE/ADD edges between the same pair must count once.
        let d = detail(
            &["a", "b"],
            vec![
                edge("a", "b", EdgeVerdict::Agree, EdgeSource::Both),
                edge("a", "b", EdgeVerdict::Add, EdgeSource::Symbolgraph),
            ],
        );
        assert_eq!(topo_order(&d).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn topo_order_detects_a_proven_cycle() {
        let d = detail(
            &["a", "b"],
            vec![
                edge("a", "b", EdgeVerdict::Agree, EdgeSource::Both),
                edge("b", "a", EdgeVerdict::Agree, EdgeSource::Both),
            ],
        );
        let cycle = topo_order(&d).unwrap_err();
        assert!(cycle.contains(&"a".to_string()));
        assert!(cycle.contains(&"b".to_string()));
    }

    #[test]
    fn prereqs_of_takes_agree_and_add_excludes_drop() {
        let d = detail(
            &["base", "mid", "top"],
            vec![
                edge("base", "mid", EdgeVerdict::Agree, EdgeSource::Both),
                edge("mid", "top", EdgeVerdict::Agree, EdgeSource::Both),
                edge("base", "top", EdgeVerdict::Drop, EdgeSource::Intent),
                edge("mid", "top", EdgeVerdict::Add, EdgeSource::Symbolgraph),
            ],
        );
        // top's prereqs: mid (agree + add, deduped) — NOT base (its edge is DROP).
        assert_eq!(prereqs_of(&d, "top"), vec!["mid"]);
        assert_eq!(prereqs_of(&d, "mid"), vec!["base"]);
        assert!(prereqs_of(&d, "base").is_empty());
    }

    // --- outer-loop tests -------------------------------------------------

    #[test]
    fn green_dag_lands_and_executes_in_topo_order() {
        let d = detail(
            &["core", "wrapper"],
            vec![edge(
                "core",
                "wrapper",
                EdgeVerdict::Agree,
                EdgeSource::Both,
            )],
        );
        let mut r = Script::new(green_split(d));
        r.rev = Some(reverify_outcome(2));
        r.execs.insert("core".into(), exec(1, 2, 2, 100, 3));
        r.execs.insert("wrapper".into(), exec(2, 3, 2, 200, 4));

        let res = drive_goal("build X", &mut r).unwrap();

        assert_eq!(res.phase, GoalPhase::Landed);
        assert!(res.is_landed());
        assert!(!res.escalated);
        assert!(res.review.is_empty());
        assert_eq!(res.order, vec!["core", "wrapper"]);
        // executed once each, in dependency order.
        assert_eq!(r.calls.len(), 2);
        assert_eq!(r.calls[0].id, "core");
        assert_eq!(r.calls[0].order_index, 0);
        assert_eq!(r.calls[1].id, "wrapper");
        assert_eq!(r.calls[1].prereqs, vec!["core"]);
        assert_eq!(r.calls[1].touched_files, vec!["wrapper.py".to_string()]);
        // records mirror the executor output.
        assert_eq!(res.exec.len(), 2);
        assert_eq!(res.exec[1].patches, 2);
        // telemetry stitched: reverify_size from T2, patches/tokens/accept summed.
        assert_eq!(res.telemetry.reverify_size, 2);
        assert_eq!(res.telemetry.patches, 3); // 1 + 2
        assert_eq!(res.telemetry.tokens, 300); // 100 + 200
        assert_eq!(res.telemetry.tool_calls, 7); // 3 + 4
        assert!((res.telemetry.accept_rate - 4.0 / 5.0).abs() < 1e-9); // 4/5 accepted
        assert_eq!(
            res.reverify.as_ref().map(|o| o.state),
            Some(OutcomeState::Green)
        );
    }

    #[test]
    fn escalates_on_conflict_without_executing() {
        let d = detail(
            &["a", "b"],
            vec![edge("a", "b", EdgeVerdict::Conflict, EdgeSource::Intent)],
        );
        let mut r = Script::new(blocked_review(d, vec!["a → b (conflict)"]));

        let res = drive_goal("g", &mut r).unwrap();

        assert_eq!(res.phase, GoalPhase::Escalated);
        assert!(res.escalated);
        assert_eq!(res.review, vec!["a → b (conflict)"]);
        assert!(res.order.is_empty());
        assert!(res.exec.is_empty());
        assert!(res.reverify.is_none());
        assert!(r.calls.is_empty(), "no task should execute past the gate");
        // The split envelope is preserved verbatim for the reviewer.
        assert_eq!(res.split.state, OutcomeState::Blocked);
        assert_eq!(res.split.reason_code, reason::NEEDS_REVIEW);
    }

    #[test]
    fn escalates_on_unknown_edge() {
        let d = detail(
            &["a", "b"],
            vec![edge("a", "b", EdgeVerdict::Unknown, EdgeSource::Intent)],
        );
        let mut r = Script::new(blocked_review(d, vec!["a → b (unknown)"]));
        let res = drive_goal("g", &mut r).unwrap();
        assert!(res.escalated);
        assert!(r.calls.is_empty());
    }

    #[test]
    fn escalates_on_a_proven_cycle_even_when_green() {
        // The adjudicator settled every edge (Green), but the proven edges form
        // a cycle — the loop refuses to pick an arbitrary order (§7).
        let d = detail(
            &["a", "b"],
            vec![
                edge("a", "b", EdgeVerdict::Agree, EdgeSource::Both),
                edge("b", "a", EdgeVerdict::Agree, EdgeSource::Both),
            ],
        );
        let mut r = Script::new(green_split(d));
        let res = drive_goal("g", &mut r).unwrap();
        assert_eq!(res.phase, GoalPhase::Escalated);
        assert_eq!(res.review.len(), 1);
        assert!(res.review[0].contains("proven dependency cycle"));
        assert!(r.calls.is_empty());
        assert!(res.reverify.is_none());
    }

    #[test]
    fn escalates_when_adjudicator_is_green_but_not_a_split() {
        // An internally inconsistent envelope is a bug, never guessed around.
        let mut r = Script::new(ToolOutcome::green(TOOL_GOAL, TypedDetail::None));
        let res = drive_goal("mygoal", &mut r).unwrap();
        assert_eq!(res.phase, GoalPhase::Escalated);
        assert!(res.escalated);
        assert_eq!(res.goal, "mygoal"); // fell back to the intent goal
        assert!(res.review[0].contains(reason::INTERNAL));
        assert!(r.calls.is_empty());
    }

    #[test]
    fn drops_do_not_add_a_prereq_but_the_task_still_runs() {
        let d = detail(
            &["base", "mid", "top"],
            vec![
                edge("base", "mid", EdgeVerdict::Agree, EdgeSource::Both),
                edge("mid", "top", EdgeVerdict::Agree, EdgeSource::Both),
                edge("base", "top", EdgeVerdict::Drop, EdgeSource::Intent),
            ],
        );
        let mut r = Script::new(green_split(d));
        let res = drive_goal("g", &mut r).unwrap();
        assert_eq!(res.phase, GoalPhase::Landed);
        assert_eq!(res.order, vec!["base", "mid", "top"]);
        let top = r.calls.iter().find(|c| c.id == "top").unwrap();
        assert_eq!(top.prereqs, vec!["mid"]); // base's DROP edge excluded
    }

    #[test]
    fn lands_with_no_reverify_set() {
        let d = detail(&["a"], vec![]);
        let mut r = Script::new(green_split(d));
        r.rev = None; // caller passed no base/head → no land-time set computed
        let res = drive_goal("g", &mut r).unwrap();
        assert_eq!(res.phase, GoalPhase::Landed);
        assert!(res.reverify.is_none());
        assert_eq!(res.telemetry.reverify_size, 0);
    }

    #[test]
    fn plan_error_aborts_the_whole_loop() {
        let mut r = Script::new(green_split(detail(&["a"], vec![])));
        r.plan_err = true;
        assert!(drive_goal("g", &mut r).is_err());
        assert!(r.calls.is_empty());
    }

    #[test]
    fn execute_error_aborts_the_loop() {
        let d = detail(
            &["a", "b"],
            vec![edge("a", "b", EdgeVerdict::Agree, EdgeSource::Both)],
        );
        let mut r = Script::new(green_split(d));
        r.exec_err_on = Some("b".into());
        assert!(drive_goal("g", &mut r).is_err());
        // 'a' ran before 'b' failed.
        assert_eq!(
            r.calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn goal_result_serializes_with_lowercase_phase() {
        let d = detail(&["a"], vec![]);
        let mut r = Script::new(green_split(d));
        let res = drive_goal("g", &mut r).unwrap();
        let j = serde_json::to_value(&res).unwrap();
        assert_eq!(j["phase"], "landed");
        assert_eq!(j["escalated"], false);
        // empty collections are skipped.
        assert!(j.get("review").is_none());
        assert!(j["telemetry"]["version"].as_u64().is_some());
    }

    #[test]
    fn empty_goal_lands_immediately() {
        let mut r = Script::new(green_split(detail(&[], vec![])));
        let res = drive_goal("g", &mut r).unwrap();
        assert_eq!(res.phase, GoalPhase::Landed);
        assert!(res.order.is_empty());
        assert!(r.calls.is_empty());
    }

    // --- parallel schedule (G8/P1) ---------------------------------------

    #[test]
    fn serial_drive_attaches_no_schedule() {
        // Without an oracle, the loop behaves exactly as before and never
        // populates the schedule hint.
        let d = detail(&["a"], vec![]);
        let mut r = Script::new(green_split(d));
        let res = drive_goal("g", &mut r).unwrap();
        assert!(res.schedule.is_none());
    }

    #[test]
    fn parallel_drive_batches_disjoint_files_and_still_executes_all() {
        // Two independent tasks touch disjoint files (a.py / b.py) — the file
        // oracle proves them safe to co-schedule, so they land in one batch.
        let d = detail(&["a", "b"], vec![]);
        let mut r = Script::new(green_split(d));
        let oracle = crate::parallel::FileOverlapOracle;
        let res = drive_goal_parallel("g", &mut r, &oracle).unwrap();

        assert_eq!(res.phase, GoalPhase::Landed);
        let sched = res
            .schedule
            .as_ref()
            .expect("parallel drive sets a schedule");
        assert_eq!(sched.batches.len(), 1, "disjoint files share one batch");
        assert_eq!(sched.max_width, 2);
        assert!(!sched.degraded);
        // every task still executes exactly once, order is the batch-major flatten.
        assert_eq!(res.order, vec!["a", "b"]);
        assert_eq!(
            r.calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn parallel_drive_serializes_a_dependency_across_batches() {
        // a → b (AGREE): a real prereq. b cannot share a's batch.
        let d = detail(
            &["a", "b"],
            vec![edge("a", "b", EdgeVerdict::Agree, EdgeSource::Both)],
        );
        let mut r = Script::new(green_split(d));
        let oracle = crate::parallel::FileOverlapOracle;
        let res = drive_goal_parallel("g", &mut r, &oracle).unwrap();

        let sched = res.schedule.as_ref().unwrap();
        assert_eq!(sched.batches.len(), 2, "a prereq forces two batches");
        assert_eq!(sched.batches[0].tasks, vec!["a"]);
        assert_eq!(sched.batches[1].tasks, vec!["b"]);
        assert_eq!(res.order, vec!["a", "b"]);
    }

    #[test]
    fn parallel_drive_degrades_to_serial_on_unknown_footprint() {
        // A task with no known touched files cannot be *proven* independent, so
        // the oracle serializes it and the schedule is flagged degraded (§7) —
        // never an optimistic parallel guess.
        let mut d = detail(&["a", "b"], vec![]);
        d.nodes[0].touched_files.clear(); // 'a' has an unknown footprint.
        let mut r = Script::new(green_split(d));
        let oracle = crate::parallel::FileOverlapOracle;
        let res = drive_goal_parallel("g", &mut r, &oracle).unwrap();

        let sched = res.schedule.as_ref().unwrap();
        assert!(sched.degraded, "unknown footprint degrades the schedule");
        assert_eq!(sched.max_width, 1, "degrade means fully serial");
        assert_eq!(sched.batches.len(), 2);
        // still lands and runs both, just serially.
        assert_eq!(res.phase, GoalPhase::Landed);
        assert_eq!(r.calls.len(), 2);
    }

    // --- P2 (G8) re-plan loop --------------------------------------------
    //
    // The re-plan loop reacts to an *observed* footprint that escapes the set
    // the GoalGraph attributed to a task (drift). It folds the drift back in,
    // re-adjudicates the remaining tasks, and restarts the walk — capping the
    // number of re-plans (§3.3) and always keeping a resumable checkpoint of
    // the work already landed. The caller's scripts drift per task
    // (`observe_drift`) and the outcome of each *re*-adjudication
    // (`replan_adj`, consumed in order after the initial pass).

    #[test]
    fn no_drift_lands_without_a_checkpoint() {
        // The plain single-pass path: `observe` returns None for every task, so
        // nothing drifts and the landed result carries no checkpoint (pre-P2
        // behaviour, byte-for-byte).
        let d = detail(&["a", "b"], vec![]);
        let mut r = Script::new(green_split(d));
        let res = drive_goal("g", &mut r).unwrap();

        assert_eq!(res.phase, GoalPhase::Landed);
        assert!(
            res.checkpoint.is_none(),
            "no drift ⇒ no checkpoint: {res:?}"
        );
        // adjudicate was called exactly once — the loop never re-planned.
        assert_eq!(r.adj_idx, 1);
        assert_eq!(res.order, vec!["a", "b"]);
    }

    #[test]
    fn observed_footprint_within_attribution_is_not_drift() {
        // `observe` returns Some(files) but every file is inside the attributed
        // set — that is a confirmation, not drift, so still no re-plan.
        let d = detail(&["a", "b"], vec![]);
        let mut r = Script::new(green_split(d));
        // a.py is exactly what the GoalGraph attributed to `a` (node builder).
        r.observe_drift.insert("a".into(), vec!["a.py".into()]);
        r.observe_drift.insert("b".into(), vec!["./b.py".into()]); // normalized match
        let res = drive_goal("g", &mut r).unwrap();

        assert_eq!(res.phase, GoalPhase::Landed);
        assert!(res.checkpoint.is_none(), "in-set observation ⇒ no drift");
        assert_eq!(r.adj_idx, 1, "no re-adjudication happened");
    }

    #[test]
    fn explainable_drift_replans_once_and_lands_with_a_checkpoint() {
        // `a` executes, but the launcher observed it also touched `shared.py`
        // (outside its attributed a.py). That drift folds back in and the
        // remaining task is re-adjudicated Green, the walk restarts skipping
        // the already-landed `a`, and the goal lands with a checkpoint of one
        // re-plan.
        let d = detail(&["a", "b"], vec![]);
        let mut r = Script::new(green_split(d.clone()));
        // The re-adjudication settles Green again over the same universe.
        r.replan_adj.push(green_split(d));
        r.observe_drift
            .insert("a".into(), vec!["a.py".into(), "shared.py".into()]);
        // `b` stays within its footprint — no cascade.
        r.observe_drift.insert("b".into(), vec!["b.py".into()]);

        let res = drive_goal("g", &mut r).unwrap();

        assert_eq!(
            res.phase,
            GoalPhase::Landed,
            "drift re-settled Green: {res:?}"
        );
        assert!(!res.escalated);
        // Both tasks executed exactly once; `a` is never redone after the re-plan.
        assert_eq!(
            r.calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(res.order, vec!["a", "b"]);
        // adjudicate: 1 initial + 1 re-plan.
        assert_eq!(r.adj_idx, 2);
        let cp = res
            .checkpoint
            .as_ref()
            .expect("re-plan attaches a checkpoint");
        assert_eq!(cp.replans, 1);
        assert_eq!(cp.completed, vec!["a", "b"]);
    }

    #[test]
    fn drift_that_reopens_review_escalates_keeping_the_checkpoint() {
        // The drift, once folded in, makes the re-adjudication surface an
        // unresolved edge (Blocked). shuttle refuses to guess past it (§7) — it
        // escalates mid-execution, but keeps the checkpoint so the already
        // landed `a` is never redone on resume.
        let d = detail(&["a", "b"], vec![]);
        let mut r = Script::new(green_split(d.clone()));
        // Re-adjudication is a blocked review naming the conflict.
        r.replan_adj
            .push(blocked_review(d, vec!["a ↔ b (shared symbol)"]));
        r.observe_drift
            .insert("a".into(), vec!["a.py".into(), "b.py".into()]);

        let res = drive_goal("g", &mut r).unwrap();

        assert_eq!(res.phase, GoalPhase::Escalated);
        assert!(res.escalated);
        assert_eq!(res.review, vec!["a ↔ b (shared symbol)"]);
        // `a` had already executed and landed; `b` never ran past the gate.
        assert_eq!(
            r.calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["a"]
        );
        // The checkpoint records the landed prefix so a resume skips it.
        let cp = res
            .checkpoint
            .as_ref()
            .expect("mid-exec escalation keeps a checkpoint");
        assert_eq!(cp.completed, vec!["a"]);
        assert_eq!(cp.replans, 1);
        // A mid-exec escalation carries the exec record of the landed prefix
        // and no reverify / schedule.
        assert_eq!(res.exec.len(), 1);
        assert_eq!(res.exec[0].id, "a");
        assert!(res.reverify.is_none());
        assert!(res.schedule.is_none());
    }

    #[test]
    fn drift_whose_replan_cycles_escalates_keeping_the_checkpoint() {
        // The re-adjudication settles Green but the proven edges now form a
        // cycle — the loop refuses an arbitrary order and escalates, still
        // holding the checkpoint of the landed prefix.
        let d = detail(&["a", "b"], vec![]);
        let mut r = Script::new(green_split(d));
        // Re-plan is Green but cyclic (a→b and b→a both AGREE).
        let cyclic = detail(
            &["a", "b"],
            vec![
                edge("a", "b", EdgeVerdict::Agree, EdgeSource::Both),
                edge("b", "a", EdgeVerdict::Agree, EdgeSource::Both),
            ],
        );
        r.replan_adj.push(green_split(cyclic));
        r.observe_drift
            .insert("a".into(), vec!["a.py".into(), "drifted.py".into()]);

        let res = drive_goal("g", &mut r).unwrap();

        assert_eq!(res.phase, GoalPhase::Escalated);
        assert_eq!(res.review.len(), 1);
        assert!(res.review[0].contains("cycle"), "review: {:?}", res.review);
        let cp = res.checkpoint.as_ref().unwrap();
        assert_eq!(cp.completed, vec!["a"]);
        assert_eq!(cp.replans, 1);
    }

    #[test]
    fn drift_replanned_into_a_non_split_envelope_escalates() {
        // A re-adjudication that returns Green-but-not-a-Split is an internal
        // inconsistency; the mid-exec path escalates with the INTERNAL reason
        // and keeps the checkpoint.
        let d = detail(&["a", "b"], vec![]);
        let mut r = Script::new(green_split(d));
        r.replan_adj
            .push(ToolOutcome::green(TOOL_GOAL, TypedDetail::None));
        r.observe_drift
            .insert("a".into(), vec!["a.py".into(), "x.py".into()]);

        let res = drive_goal("g", &mut r).unwrap();

        assert_eq!(res.phase, GoalPhase::Escalated);
        assert!(res.review.iter().any(|m| m.contains(reason::INTERNAL)));
        assert_eq!(res.checkpoint.as_ref().unwrap().completed, vec!["a"]);
    }

    #[test]
    fn repeated_drift_hits_the_replan_cap_and_escalates() {
        // Four tasks each drift once. The cap (MAX_REPLAN = 3) admits three
        // re-plans; the fourth drift trips the anti-thrash guard and escalates
        // — keeping the checkpoint of everything landed so far (§3.3).
        let d = detail(&["a", "b", "c", "e"], vec![]);
        let mut r = Script::new(green_split(d.clone()));
        // Every re-adjudication re-settles Green over the same four-task universe.
        for _ in 0..MAX_REPLAN {
            r.replan_adj.push(green_split(d.clone()));
        }
        // Each task is observed to touch a file outside its attribution.
        r.observe_drift
            .insert("a".into(), vec!["a.py".into(), "d1.py".into()]);
        r.observe_drift
            .insert("b".into(), vec!["b.py".into(), "d2.py".into()]);
        r.observe_drift
            .insert("c".into(), vec!["c.py".into(), "d3.py".into()]);
        r.observe_drift
            .insert("e".into(), vec!["e.py".into(), "d4.py".into()]);

        let res = drive_goal("g", &mut r).unwrap();

        assert_eq!(res.phase, GoalPhase::Escalated);
        assert!(res.escalated);
        assert_eq!(res.review.len(), 1);
        assert!(
            res.review[0].contains(&format!("re-plan cap ({MAX_REPLAN})")),
            "cap review must name the limit: {:?}",
            res.review
        );
        // The three admitted re-plans plus the initial pass = 4 adjudications.
        assert_eq!(r.adj_idx, MAX_REPLAN + 1);
        // All four tasks executed once (in topo/id order) before the cap tripped.
        assert_eq!(
            r.calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c", "e"]
        );
        let cp = res.checkpoint.as_ref().unwrap();
        assert_eq!(cp.replans, MAX_REPLAN);
        assert_eq!(cp.completed, vec!["a", "b", "c", "e"]);
    }

    #[test]
    fn replan_drift_normalizes_dot_slash_paths() {
        // The observed footprint uses a `./`-prefixed path that, once
        // normalized, is inside the attribution — so it is NOT drift. This pins
        // that drift detection normalizes exactly like the adjudicator.
        let d = detail(&["a"], vec![]);
        let mut r = Script::new(green_split(d));
        r.observe_drift.insert("a".into(), vec!["./a.py".into()]);
        let res = drive_goal("g", &mut r).unwrap();
        assert_eq!(res.phase, GoalPhase::Landed);
        assert!(res.checkpoint.is_none(), "./a.py ≡ a.py ⇒ no drift");
        assert_eq!(r.adj_idx, 1);
    }

    #[test]
    fn parallel_drive_replans_on_drift_and_reschedules() {
        // The re-plan loop is oracle-aware: when scheduling, a drift re-plan
        // recomputes the batch partition over the re-adjudicated DAG. Two
        // disjoint tasks batch wide; `a` drifts once, re-settles Green, and the
        // goal lands with a fresh schedule and a one-re-plan checkpoint.
        let d = detail(&["a", "b"], vec![]);
        let mut r = Script::new(green_split(d.clone()));
        r.replan_adj.push(green_split(d));
        r.observe_drift
            .insert("a".into(), vec!["a.py".into(), "shared.py".into()]);
        let oracle = crate::parallel::FileOverlapOracle;

        let res = drive_goal_parallel("g", &mut r, &oracle).unwrap();

        assert_eq!(res.phase, GoalPhase::Landed);
        let sched = res.schedule.as_ref().expect("re-plan keeps a schedule");
        assert!(!sched.batches.is_empty());
        let cp = res.checkpoint.as_ref().unwrap();
        assert_eq!(cp.replans, 1);
        assert_eq!(cp.completed, vec!["a", "b"]);
    }

    // --- P2b (G8) `--resume` ---------------------------------------------
    //
    // `drive_goal_resumable` threads a prior [`GoalCheckpoint`] in: its
    // completed tasks are treated as already landed (never redone) and the
    // re-plan tally continues (anti-thrash across resumes, §3.3). shuttle owns no
    // checkpoint store (#11) — the caller persists it and hands it back.

    #[test]
    fn resume_skips_completed_tasks_and_runs_only_the_tail() {
        // A prior session landed `a`; the resume replays only `b`.
        let d = detail(&["a", "b"], vec![]);
        let mut r = Script::new(green_split(d));
        let cp = GoalCheckpoint {
            completed: vec!["a".into()],
            replans: 0,
        };
        let res = drive_goal_resumable("g", &mut r, None, Some(cp)).unwrap();

        assert_eq!(res.phase, GoalPhase::Landed);
        // Only `b` executed — `a` is never redone.
        assert_eq!(
            r.calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["b"]
        );
        // The completed order carries the resumed prefix + the tail.
        assert_eq!(res.order, vec!["a", "b"]);
        // A resumed finish still records the final landed checkpoint.
        let out = res
            .checkpoint
            .as_ref()
            .expect("resume attaches a checkpoint");
        assert_eq!(out.completed, vec!["a", "b"]);
        assert_eq!(out.replans, 0);
    }

    #[test]
    fn resume_with_everything_completed_executes_nothing() {
        // Both tasks were already landed — the resume is a no-op walk that
        // still lands (idempotent) and redoes nothing.
        let d = detail(&["a", "b"], vec![]);
        let mut r = Script::new(green_split(d));
        let cp = GoalCheckpoint {
            completed: vec!["a".into(), "b".into()],
            replans: 1,
        };
        let res = drive_goal_resumable("g", &mut r, None, Some(cp)).unwrap();

        assert_eq!(res.phase, GoalPhase::Landed);
        assert!(r.calls.is_empty(), "nothing left to run: {:?}", r.calls);
        assert_eq!(res.order, vec!["a", "b"]);
        // The prior re-plan tally is preserved across the resume.
        assert_eq!(res.checkpoint.as_ref().unwrap().replans, 1);
    }

    #[test]
    fn resume_continues_the_replan_tally_toward_the_cap() {
        // A prior session already spent MAX_REPLAN-1 re-plans. On resume, the
        // FIRST fresh drift admits the final re-plan; the SECOND trips the cap
        // — proving the budget is cumulative across resumes (anti-thrash).
        let d = detail(&["a", "b"], vec![]);
        let mut r = Script::new(green_split(d.clone()));
        // Two re-adjudications available, both Green over the same universe.
        r.replan_adj.push(green_split(d.clone()));
        r.replan_adj.push(green_split(d));
        // Both remaining tasks drift.
        r.observe_drift
            .insert("a".into(), vec!["a.py".into(), "x.py".into()]);
        r.observe_drift
            .insert("b".into(), vec!["b.py".into(), "y.py".into()]);
        let cp = GoalCheckpoint {
            completed: Vec::new(),
            replans: MAX_REPLAN - 1,
        };

        let res = drive_goal_resumable("g", &mut r, None, Some(cp)).unwrap();

        // `a` drifted → the last admissible re-plan (tally hits MAX_REPLAN);
        // `b` drifted → cap tripped → escalate.
        assert_eq!(res.phase, GoalPhase::Escalated);
        assert!(res.review[0].contains(&format!("re-plan cap ({MAX_REPLAN})")));
        let out = res.checkpoint.as_ref().unwrap();
        assert_eq!(out.replans, MAX_REPLAN);
        // `a` landed before the cap tripped on `b`.
        assert_eq!(out.completed, vec!["a", "b"]);
    }

    #[test]
    fn resume_parallel_reschedules_only_the_tail() {
        // The resumable entry is oracle-aware: with a schedule, the tail is
        // still batch-scheduled, and the already-landed prefix is skipped.
        let d = detail(&["a", "b"], vec![]);
        let mut r = Script::new(green_split(d));
        let oracle = crate::parallel::FileOverlapOracle;
        let cp = GoalCheckpoint {
            completed: vec!["a".into()],
            replans: 0,
        };
        let res = drive_goal_resumable("g", &mut r, Some(&oracle), Some(cp)).unwrap();

        assert_eq!(res.phase, GoalPhase::Landed);
        assert!(res.schedule.is_some(), "parallel resume keeps a schedule");
        assert_eq!(
            r.calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["b"]
        );
    }

    #[test]
    fn resume_none_checkpoint_is_the_plain_path() {
        // `drive_goal_resumable(.., None)` is byte-for-byte the fresh path: no
        // resumed prefix, no forced checkpoint on a drift-free land.
        let d = detail(&["a"], vec![]);
        let mut r = Script::new(green_split(d));
        let res = drive_goal_resumable("g", &mut r, None, None).unwrap();
        assert_eq!(res.phase, GoalPhase::Landed);
        assert!(res.checkpoint.is_none());
        assert_eq!(
            r.calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["a"]
        );
    }

    #[test]
    fn resume_checkpoint_roundtrips_through_json() {
        // The checkpoint a session emits deserializes back verbatim (the
        // `--resume` file contract): Serialize + Deserialize agree.
        let cp = GoalCheckpoint {
            completed: vec!["a".into(), "b".into()],
            replans: 2,
        };
        let json = serde_json::to_string(&cp).unwrap();
        let back: GoalCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(cp, back);
    }
}

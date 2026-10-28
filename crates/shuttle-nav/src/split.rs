//! `shuttle split --correct` — the GoalGraph three-state adjudicator (T1).
//!
//! # What this is
//! A planner (pi / the model) decomposes a goal into an **intent DAG**: a set of
//! candidate tasks, each with the files it *guesses* it will touch and the tasks
//! it *guesses* it depends on. Those guesses are exactly that — guesses. This
//! module is the mechanical corrector that sits between the model's intent DAG
//! and the human/external review gate. It never re-plans; it takes the directional
//! [`SymbolGraph`] (T3) as ground truth and adjudicates *each dependency edge*
//! into one of three decidable + two escalated states:
//!
//! * **AGREE** — the model's edge is corroborated by a trusted import-directed
//!   symbol edge. High confidence (two independent sources agree) → auto-keep.
//! * **ADD** — the symbol graph *proves* a dependency the model omitted. Added
//!   with `source = symbolgraph`, never silently — it is reported as an add.
//! * **DROP** — the model declared a direct edge the symbol graph proves is
//!   *redundant* (already implied transitively). Suggested for removal with the
//!   transitive evidence; the author's intent is never overridden in place.
//! * **CONFLICT** — the model's direction contradicts a proven mechanical
//!   direction. Escalated to review — shuttle never silently flips an author edge.
//! * **UNKNOWN** — the machine can neither prove nor disprove the edge (no
//!   import corroboration, a task's files fall outside the scanned universe, or
//!   the callee is ambiguous). Escalated to review, **never guessed** (§7).
//!
//! # Never guess (§7) — the hard rule this module exists to honor
//! A static symbol graph is inherently incomplete (dynamic imports, reflection,
//! macros, config- and time-ordered coupling can all be invisible). So this
//! adjudicator is deliberately conservative:
//! * It only trusts directed import and contract edges for direction. Weak,
//!   direction-unproven `NameMatch` and `CoChange` edges contribute to
//!   *nothing* — not an add, not a conflict, not a drop.
//! * It only DROPs an author edge when the same ordering is still guaranteed
//!   transitively by the mechanical graph (a true redundancy, not a deletion of
//!   a semantic dependency the graph merely failed to see).
//! * Anything it cannot prove *or* disprove becomes UNKNOWN and surfaces on the
//!   review gate — it never quietly binds a direction or drops a "maybe".
//!
//! The output is a [`SplitDetail`] carried in a [`ToolOutcome`]: `Green` when
//! every edge was mechanically decidable (all AGREE/ADD/DROP), `Blocked` with
//! `reason = needs-review` when any CONFLICT/UNKNOWN edge needs a human/model.
//! The review surface is O(disagreements), never O(all tasks).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use shuttle_symbols::graph::{ClosureLimits, EdgeKind, GraphConfidence, SymbolGraph};
use shuttle_symbols::SymbolTable;

use shuttle_protocol::{
    reason, DerivationSymbolRef, DerivedTask, DerivedTaskEdge, EdgeSource, EdgeVerdict,
    OutcomeConfidence, SplitDerivationDetail, SplitDetail, SplitEdge, SplitNode, ToolOutcome,
    TypedDetail,
};

/// Tool name stamped into every split-produced envelope.
pub const TOOL_SPLIT: &str = "split";

/// Schema version of the intent DAG this corrector consumes (pi → shuttle). shuttle now
/// emits/round-trips v2 (optional `acceptance`/`invariants`), while still
/// parsing v1 verbatim: the new fields carry `#[serde(default)]`, so a v1 DAG
/// with no such fields deserializes unchanged (backward compatible, §4.1). The
/// corrector already tolerates a mismatched/empty version, so neither schema is
/// ever a hard failure.
pub const INTENT_VERSION: &str = "intent/v2";
/// The prior schema version, still accepted on input (see [`INTENT_VERSION`]).
pub const INTENT_VERSION_V1: &str = "intent/v1";

// ---------------------------------------------------------------------------
// Input contract (pi → shuttle): the intent DAG. `_guess` fields are explicitly the
// model's guesses — this module is what turns them into proven/typed verdicts.
// ---------------------------------------------------------------------------

/// One candidate task the planner proposed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentTask {
    /// Stable task id (referenced by other tasks' `depends_on_guess`).
    pub id: String,
    /// Human/model intent label for the task.
    #[serde(default)]
    pub intent: String,
    /// Files the planner *guesses* this task will touch.
    #[serde(default)]
    pub touched_files_guess: Vec<String>,
    /// Task ids the planner *guesses* this task depends on (prerequisites).
    #[serde(default)]
    pub depends_on_guess: Vec<String>,
    /// intent/v2 (optional, backward-compatible with v1): acceptance criteria
    /// for this task — natural-language or machine-checkable assertions the
    /// finished task must satisfy. `None` ⇒ a v1 intent that named none; it is
    /// never fabricated. Consumed by `shuttle goal --reconcile` (§4.2), ignored by
    /// the adjudicator (proving dependencies never needs acceptance text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<Vec<String>>,
    /// intent/v2 (optional): invariants the task must preserve. Same
    /// backward-compat + never-fabricate contract as `acceptance`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invariants: Option<Vec<String>>,
}

/// The planner's intent DAG. `version` should be [`INTENT_VERSION`]; an empty or
/// mismatched version is tolerated (the corrector reports it) so a slightly old
/// planner is never a hard failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentDag {
    #[serde(default)]
    pub goal: String,
    pub tasks: Vec<IntentTask>,
    #[serde(default)]
    pub version: String,
}

/// A caller-provided symbol seed. Both fields are required because a bare name
/// cannot disambiguate duplicate definitions and is therefore not evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeriveSymbolSeed {
    pub name: String,
    pub path: String,
}

/// One task in a derive request. Whole-file write declarations are explicit
/// evidence and remain distinct from the legacy planner guess field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeriveTaskInput {
    pub id: String,
    #[serde(default)]
    pub intent: String,
    #[serde(default)]
    pub write_files: Vec<String>,
    #[serde(default)]
    pub write_symbols: Vec<DeriveSymbolSeed>,
}

/// Versioned input for the advisory derive mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DerivePlan {
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub version: String,
    pub tasks: Vec<DeriveTaskInput>,
}

// ---------------------------------------------------------------------------
// Hybrid DAG v2 policy. These types intentionally live in shuttle-nav instead of
// extending the closed v1 protocol envelope. The CLI/protocol layer can adapt
// its snapshot-bound identities into this small pure input without giving nav
// write, dispatch, or landing authority.
// ---------------------------------------------------------------------------

/// Stable policy identifier bound into every Hybrid advice artifact.
pub const HYBRID_DAG_POLICY_VERSION: &str = "ea_hybrid_dag_policy/v1";

/// A task dependency edge: `from` is the prerequisite/provider and `to` is
/// the dependent/consumer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridTaskEdge {
    pub from: String,
    pub to: String,
}

/// Nav-local mirror of graph edge provenance. Conversion deliberately calls
/// [`EdgeKind::is_trusted_directed`] so import and contract evidence pass the
/// same trust gate, while name-match and co-change can never become hard.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HybridEvidenceKind {
    ImportDirected,
    ContractDirected,
    NameMatch,
    CoChange,
}

impl HybridEvidenceKind {
    fn graph_kind(self) -> EdgeKind {
        match self {
            Self::ImportDirected => EdgeKind::ImportDirected,
            Self::ContractDirected => EdgeKind::ContractDirected,
            Self::NameMatch => EdgeKind::NameMatch,
            Self::CoChange => EdgeKind::CoChange,
        }
    }

    pub fn is_trusted_directed(self) -> bool {
        self.graph_kind().is_trusted_directed()
    }
}

impl From<EdgeKind> for HybridEvidenceKind {
    fn from(value: EdgeKind) -> Self {
        match value {
            EdgeKind::ImportDirected => Self::ImportDirected,
            EdgeKind::ContractDirected => Self::ContractDirected,
            EdgeKind::NameMatch => Self::NameMatch,
            EdgeKind::CoChange => Self::CoChange,
        }
    }
}

/// Completeness/freshness of one mechanical observation. Only
/// `fresh_exact_unique` may contribute a directed hard edge.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HybridEvidenceState {
    FreshExactUnique,
    Partial,
    Unsupported,
    Timeout,
    Truncated,
    Ambiguous,
    Stale,
    Absent,
}

impl HybridEvidenceState {
    pub fn permits_hard_addition(self) -> bool {
        self == Self::FreshExactUnique
    }
}

/// A raw symbol-graph observation. SymbolGraph edges are `owner -> callee`
/// (consumer -> provider); Hybrid task dependencies reverse that orientation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridMechanicalEvidence {
    pub owner_task: String,
    pub callee_task: String,
    pub kind: HybridEvidenceKind,
    pub state: HybridEvidenceState,
    pub evidence_ref: String,
}

/// Explicit exact-symbol write claim. Multiple tasks claiming the same symbol
/// are an undirected collision; this type carries no directional field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridWriteClaim {
    pub task_id: String,
    pub symbol_id: String,
}

/// Evidence can degrade even when it yields no candidate edge (for example an
/// absent index). Callers surface those conditions explicitly here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridEvidenceIssue {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_ids: Vec<String>,
    pub state: HybridEvidenceState,
    pub evidence_ref: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HybridMergeInput {
    pub task_ids: Vec<String>,
    #[serde(default)]
    pub model_edges: Vec<HybridTaskEdge>,
    #[serde(default)]
    pub mechanical_edges: Vec<HybridMechanicalEvidence>,
    #[serde(default)]
    pub write_claims: Vec<HybridWriteClaim>,
    #[serde(default)]
    pub evidence_issues: Vec<HybridEvidenceIssue>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HybridEdgeProvenance {
    Model,
    TrustedMechanical,
    Both,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridTrustedCandidate {
    pub edge: HybridTaskEdge,
    pub kinds: Vec<HybridEvidenceKind>,
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridHardEdge {
    pub edge: HybridTaskEdge,
    pub provenance: HybridEdgeProvenance,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<HybridEvidenceKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<String>,
}

/// Weak graph evidence is represented as an unordered task pair. This avoids
/// accidentally presenting NameMatch or CoChange as a dependency direction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridWeakHint {
    pub task_a: String,
    pub task_b: String,
    pub kind: HybridEvidenceKind,
    pub evidence_ref: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HybridConflictKind {
    ReverseModelEdge,
    CycleProducingAddition,
    ModelCycle,
    WriteWriteCollision,
    AmbiguousEvidence,
    InvalidTaskReference,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridConflict {
    pub kind: HybridConflictKind,
    pub task_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge: Option<HybridTaskEdge>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridDegradation {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_ids: Vec<String>,
    pub state: HybridEvidenceState,
    pub evidence_ref: String,
}

/// Verdict of the automatic graph-assisted adoption gate. `Adopt` means every
/// structural gate passed and a consumer *may* adopt the proposed hard DAG;
/// `Block` means at least one gate failed and adoption is withheld. This is
/// advice, never authorization: shuttle proposes and proves, it never claims a
/// workspace, dispatches, or lands (red-line #11).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HybridAdoptionVerdict {
    /// All gates green — the proposed hard DAG is internally safe to adopt.
    Adopt,
    /// One or more gates failed — see [`HybridAdoptionDecision::reasons`].
    Block,
}

/// Machine-enumerable code for *why* adoption was blocked. Each maps 1:1 to one
/// of the three orthogonal safety gates so a consumer can react per-cause
/// (e.g. re-plan on a cycle, wait for a re-index on degraded evidence) instead
/// of collapsing everything into a single opaque "blocked" bit.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HybridAdoptionReasonCode {
    /// The proposed hard DAG (model edges + trusted mechanical additions) is
    /// cyclic. Adopting it would introduce a scheduling deadlock.
    CyclicHardDag,
    /// Structural conflicts remain (reverse edge, write/write collision,
    /// invalid task reference, ambiguous evidence, or a model/addition cycle).
    UnresolvedConflicts,
    /// One or more observations degraded (stale / absent / partial / timeout /
    /// truncated / ambiguous), so mechanical edges cannot be trusted wholesale.
    DegradedEvidence,
}

/// One blocking cause plus the count of underlying items it summarizes
/// (conflicts / degradations; `0` for the purely structural cycle gate).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct HybridAdoptionReason {
    pub code: HybridAdoptionReasonCode,
    /// How many underlying artifacts drove this reason. For `CyclicHardDag`
    /// this is always `0` (it is a single structural fact); for the other two
    /// it is the number of conflicts / degradations respectively.
    #[serde(default)]
    pub count: usize,
}

/// The adoption gate's structured decision. Derived purely from the merge's own
/// `acyclic` / `conflicts` / `degradations`, so it is a deterministic function
/// of the DAG and never introduces a second source of truth. `reasons` is empty
/// iff `verdict == Adopt`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HybridAdoptionDecision {
    pub verdict: HybridAdoptionVerdict,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<HybridAdoptionReason>,
}

impl HybridAdoptionDecision {
    /// Compute the decision from the three orthogonal safety gates. Reasons are
    /// emitted in a stable order (cycle, conflicts, degradations) and the sort
    /// keeps output byte-identical regardless of caller ordering.
    pub fn from_gates(acyclic: bool, conflict_count: usize, degradation_count: usize) -> Self {
        let mut reasons = Vec::new();
        if !acyclic {
            reasons.push(HybridAdoptionReason {
                code: HybridAdoptionReasonCode::CyclicHardDag,
                count: 0,
            });
        }
        if conflict_count > 0 {
            reasons.push(HybridAdoptionReason {
                code: HybridAdoptionReasonCode::UnresolvedConflicts,
                count: conflict_count,
            });
        }
        if degradation_count > 0 {
            reasons.push(HybridAdoptionReason {
                code: HybridAdoptionReasonCode::DegradedEvidence,
                count: degradation_count,
            });
        }
        reasons.sort();
        let verdict = if reasons.is_empty() {
            HybridAdoptionVerdict::Adopt
        } else {
            HybridAdoptionVerdict::Block
        };
        Self { verdict, reasons }
    }

    /// Whether adoption is recommended. Convenience mirror of the verdict.
    pub fn is_adopt(&self) -> bool {
        matches!(self.verdict, HybridAdoptionVerdict::Adopt)
    }
}

/// Deterministic output of the monotonic Hybrid merge. It is advice only: no
/// workspace, resource-claim, approval, dispatch, or landing authority is
/// represented.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HybridDag {
    pub policy_version: String,
    pub task_ids: Vec<String>,
    pub model_edges: Vec<HybridTaskEdge>,
    pub trusted_addition_candidates: Vec<HybridTrustedCandidate>,
    pub weak_hints: Vec<HybridWeakHint>,
    pub conflicts: Vec<HybridConflict>,
    pub proposed_hard_edges: Vec<HybridHardEdge>,
    pub degradations: Vec<HybridDegradation>,
    pub acyclic: bool,
    /// Back-compat convenience: `true` iff [`Self::adoption`] verdict is
    /// `Adopt`. Kept in lock-step with `adoption` so older consumers that read
    /// a single bool still see the identical decision.
    pub automatic_adoption_allowed: bool,
    /// Structured adoption gate decision (verdict + enumerated reasons). This
    /// is the richer, machine-consumable form of `automatic_adoption_allowed`.
    pub adoption: HybridAdoptionDecision,
}

fn degraded_derivation(
    plan: &DerivePlan,
    index_status: &str,
    max_depth: usize,
    max_fanout: usize,
) -> SplitDerivationDetail {
    let mut tasks: Vec<DerivedTask> = plan
        .tasks
        .iter()
        .map(|task| DerivedTask {
            id: task.id.clone(),
            write_symbols: Vec::new(),
            read_symbols: Vec::new(),
            touch_hint: task.write_files.iter().map(|path| norm(path)).collect(),
        })
        .collect();
    for task in &mut tasks {
        task.touch_hint.sort();
        task.touch_hint.dedup();
    }
    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    SplitDerivationDetail {
        goal: plan.goal.clone(),
        tasks,
        hard_edges: Vec::new(),
        weak_hints: Vec::new(),
        index_status: index_status.to_string(),
        max_depth,
        max_fanout,
        truncated: false,
        truncation_reasons: Vec::new(),
        acyclic: true,
        fallback_required: true,
        degraded_reason: Some(format!("index-{index_status}")),
    }
}

// ---------------------------------------------------------------------------
// Path helpers (kept local so split.rs does not couple to lib-private fns).
// ---------------------------------------------------------------------------

/// Normalize a path for comparison: `\\` → `/`, strip a leading `./`.
fn norm(p: &str) -> String {
    let p = p.replace('\\', "/");
    p.strip_prefix("./").unwrap_or(&p).to_string()
}

/// A symbol's def file `sym_path` belongs to a task's `files` when the two are
/// equal or one is a `/`-boundary suffix of the other (handles repo-relative vs
/// differently-rooted ctags paths — same rule shuttle-nav's impact uses).
fn path_owned_by(sym_path: &str, files: &BTreeSet<String>) -> bool {
    let sp = norm(sym_path);
    files.iter().any(|w| {
        let w = norm(w);
        sp == w || sp.ends_with(&format!("/{w}")) || w.ends_with(&format!("/{sp}"))
    })
}

// ---------------------------------------------------------------------------
// Reachability (transitive closure) over a directed edge set.
// Edges are oriented (from = prerequisite, to = dependent); a pair (a,b) in the
// result means "a is a transitive prerequisite of b".
// ---------------------------------------------------------------------------

fn reachable_pairs(
    edges: &BTreeSet<(String, String)>,
    nodes: &[String],
) -> BTreeSet<(String, String)> {
    let mut adj: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (a, b) in edges {
        adj.entry(a.as_str()).or_default().push(b.as_str());
    }
    let mut out: BTreeSet<(String, String)> = BTreeSet::new();
    for start in nodes {
        let mut stack: Vec<&str> = vec![start.as_str()];
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        while let Some(n) = stack.pop() {
            if let Some(ns) = adj.get(n) {
                for &m in ns {
                    if seen.insert(m) {
                        out.insert((start.clone(), m.to_string()));
                        stack.push(m);
                    }
                }
            }
        }
    }
    out
}

fn cycle_nodes(
    declared_nodes: &BTreeSet<String>,
    edges: &BTreeSet<(String, String)>,
) -> BTreeSet<String> {
    let mut indegree: BTreeMap<String, usize> = declared_nodes
        .iter()
        .map(|node| (node.clone(), 0))
        .collect();
    let mut outgoing: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (from, to) in edges {
        indegree.entry(from.clone()).or_insert(0);
        indegree.entry(to.clone()).or_insert(0);
        if outgoing.entry(from.clone()).or_default().insert(to.clone()) {
            *indegree.entry(to.clone()).or_default() += 1;
        }
    }
    let mut ready: BTreeSet<String> = indegree
        .iter()
        .filter_map(|(node, degree)| (*degree == 0).then_some(node.clone()))
        .collect();
    while let Some(node) = ready.pop_first() {
        if let Some(next) = outgoing.get(&node) {
            for dependent in next {
                let degree = indegree
                    .get_mut(dependent)
                    .expect("edge endpoint is present");
                *degree -= 1;
                if *degree == 0 {
                    ready.insert(dependent.clone());
                }
            }
        }
    }
    indegree
        .into_iter()
        .filter_map(|(node, degree)| (degree != 0).then_some(node))
        .collect()
}

fn has_path(
    edges: &BTreeSet<(String, String)>,
    start: &str,
    target: &str,
    excluded: Option<(&str, &str)>,
) -> bool {
    if start == target {
        return true;
    }
    let mut outgoing: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (from, to) in edges {
        if excluded.is_some_and(|edge| edge == (from.as_str(), to.as_str())) {
            continue;
        }
        outgoing.entry(from.as_str()).or_default().push(to.as_str());
    }
    let mut seen = BTreeSet::new();
    let mut stack = vec![start];
    while let Some(node) = stack.pop() {
        if let Some(next) = outgoing.get(node) {
            for dependent in next {
                if *dependent == target {
                    return true;
                }
                if seen.insert(*dependent) {
                    stack.push(dependent);
                }
            }
        }
    }
    false
}

fn normalized_task_ids(ids: &[String]) -> Vec<String> {
    ids.iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Monotonically merge model dependencies with fresh, exact, trusted-directed
/// mechanical evidence. The function is pure and order-independent. It never
/// removes or flips a model edge, never directs a weak hint/collision, and
/// rejects every mechanical addition participating in a candidate cycle.
pub fn merge_hybrid_dag(input: &HybridMergeInput) -> HybridDag {
    let task_ids = normalized_task_ids(&input.task_ids);
    let declared: BTreeSet<String> = task_ids.iter().cloned().collect();
    let mut conflicts = BTreeSet::new();
    let mut degradations = BTreeSet::new();

    if task_ids.len() != input.task_ids.len()
        || input.task_ids.iter().any(|id| id.trim().is_empty())
    {
        conflicts.insert(HybridConflict {
            kind: HybridConflictKind::AmbiguousEvidence,
            task_ids: Vec::new(),
            edge: None,
            symbol_id: None,
            evidence_refs: vec!["task_ids".to_string()],
        });
    }

    let model_edges: Vec<HybridTaskEdge> = input
        .model_edges
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let model_pairs: BTreeSet<(String, String)> = model_edges
        .iter()
        .map(|edge| (edge.from.clone(), edge.to.clone()))
        .collect();
    for edge in &model_edges {
        if !declared.contains(&edge.from) || !declared.contains(&edge.to) {
            conflicts.insert(HybridConflict {
                kind: HybridConflictKind::InvalidTaskReference,
                task_ids: vec![edge.from.clone(), edge.to.clone()],
                edge: Some(edge.clone()),
                symbol_id: None,
                evidence_refs: vec!["model_edge".to_string()],
            });
        }
    }
    let model_cycle_nodes = cycle_nodes(&declared, &model_pairs);
    if !model_cycle_nodes.is_empty() {
        conflicts.insert(HybridConflict {
            kind: HybridConflictKind::ModelCycle,
            task_ids: model_cycle_nodes.into_iter().collect(),
            edge: None,
            symbol_id: None,
            evidence_refs: Vec::new(),
        });
    }

    for issue in &input.evidence_issues {
        let task_ids = normalized_task_ids(&issue.task_ids);
        degradations.insert(HybridDegradation {
            task_ids: task_ids.clone(),
            state: issue.state,
            evidence_ref: issue.evidence_ref.clone(),
        });
        if issue.state == HybridEvidenceState::Ambiguous {
            conflicts.insert(HybridConflict {
                kind: HybridConflictKind::AmbiguousEvidence,
                task_ids,
                edge: None,
                symbol_id: None,
                evidence_refs: vec![issue.evidence_ref.clone()],
            });
        }
    }

    let mut weak_hints = BTreeSet::new();
    let mut candidate_kinds: BTreeMap<HybridTaskEdge, BTreeSet<HybridEvidenceKind>> =
        BTreeMap::new();
    let mut candidate_refs: BTreeMap<HybridTaskEdge, BTreeSet<String>> = BTreeMap::new();
    for evidence in &input.mechanical_edges {
        let affected =
            normalized_task_ids(&[evidence.owner_task.clone(), evidence.callee_task.clone()]);
        let valid_tasks = declared.contains(&evidence.owner_task)
            && declared.contains(&evidence.callee_task)
            && !evidence.owner_task.trim().is_empty()
            && !evidence.callee_task.trim().is_empty();
        let edge = HybridTaskEdge {
            from: evidence.callee_task.clone(),
            to: evidence.owner_task.clone(),
        };
        if !valid_tasks {
            conflicts.insert(HybridConflict {
                kind: HybridConflictKind::InvalidTaskReference,
                task_ids: affected.clone(),
                edge: Some(edge.clone()),
                symbol_id: None,
                evidence_refs: vec![evidence.evidence_ref.clone()],
            });
        }
        if !evidence.kind.is_trusted_directed() {
            let (task_a, task_b) = if evidence.owner_task <= evidence.callee_task {
                (evidence.owner_task.clone(), evidence.callee_task.clone())
            } else {
                (evidence.callee_task.clone(), evidence.owner_task.clone())
            };
            weak_hints.insert(HybridWeakHint {
                task_a,
                task_b,
                kind: evidence.kind,
                evidence_ref: evidence.evidence_ref.clone(),
            });
            if !evidence.state.permits_hard_addition() {
                degradations.insert(HybridDegradation {
                    task_ids: affected,
                    state: evidence.state,
                    evidence_ref: evidence.evidence_ref.clone(),
                });
            }
            continue;
        }
        if !evidence.state.permits_hard_addition() || evidence.evidence_ref.trim().is_empty() {
            let state = if evidence.evidence_ref.trim().is_empty() {
                HybridEvidenceState::Absent
            } else {
                evidence.state
            };
            degradations.insert(HybridDegradation {
                task_ids: affected.clone(),
                state,
                evidence_ref: evidence.evidence_ref.clone(),
            });
            if state == HybridEvidenceState::Ambiguous {
                conflicts.insert(HybridConflict {
                    kind: HybridConflictKind::AmbiguousEvidence,
                    task_ids: affected,
                    edge: Some(edge),
                    symbol_id: None,
                    evidence_refs: vec![evidence.evidence_ref.clone()],
                });
            }
            continue;
        }
        if valid_tasks {
            candidate_kinds
                .entry(edge.clone())
                .or_default()
                .insert(evidence.kind);
            candidate_refs
                .entry(edge)
                .or_default()
                .insert(evidence.evidence_ref.clone());
        }
    }

    let trusted_addition_candidates: Vec<HybridTrustedCandidate> = candidate_kinds
        .iter()
        .map(|(edge, kinds)| HybridTrustedCandidate {
            edge: edge.clone(),
            kinds: kinds.iter().copied().collect(),
            evidence_refs: candidate_refs
                .get(edge)
                .into_iter()
                .flatten()
                .cloned()
                .collect(),
        })
        .collect();

    let mut eligible_additions = BTreeSet::new();
    for candidate in candidate_kinds.keys() {
        if model_pairs.contains(&(candidate.from.clone(), candidate.to.clone())) {
            continue;
        }
        let refs: Vec<String> = candidate_refs
            .get(candidate)
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        if candidate.from == candidate.to {
            conflicts.insert(HybridConflict {
                kind: HybridConflictKind::CycleProducingAddition,
                task_ids: vec![candidate.from.clone()],
                edge: Some(candidate.clone()),
                symbol_id: None,
                evidence_refs: refs,
            });
        } else if model_pairs.contains(&(candidate.to.clone(), candidate.from.clone())) {
            conflicts.insert(HybridConflict {
                kind: HybridConflictKind::ReverseModelEdge,
                task_ids: vec![candidate.from.clone(), candidate.to.clone()],
                edge: Some(candidate.clone()),
                symbol_id: None,
                evidence_refs: refs,
            });
        } else if cycle_nodes(&declared, &model_pairs).is_empty() {
            eligible_additions.insert((candidate.from.clone(), candidate.to.clone()));
        }
    }

    let potential_edges: BTreeSet<(String, String)> =
        model_pairs.union(&eligible_additions).cloned().collect();
    let mut accepted_additions = BTreeSet::new();
    for (from, to) in &eligible_additions {
        let candidate = HybridTaskEdge {
            from: from.clone(),
            to: to.clone(),
        };
        if has_path(
            &potential_edges,
            to,
            from,
            Some((from.as_str(), to.as_str())),
        ) {
            conflicts.insert(HybridConflict {
                kind: HybridConflictKind::CycleProducingAddition,
                task_ids: vec![from.clone(), to.clone()],
                edge: Some(candidate.clone()),
                symbol_id: None,
                evidence_refs: candidate_refs
                    .get(&candidate)
                    .into_iter()
                    .flatten()
                    .cloned()
                    .collect(),
            });
        } else {
            accepted_additions.insert((from.clone(), to.clone()));
        }
    }

    let mut claims_by_symbol: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for claim in &input.write_claims {
        if !declared.contains(&claim.task_id)
            || claim.task_id.trim().is_empty()
            || claim.symbol_id.trim().is_empty()
        {
            conflicts.insert(HybridConflict {
                kind: HybridConflictKind::InvalidTaskReference,
                task_ids: normalized_task_ids(std::slice::from_ref(&claim.task_id)),
                edge: None,
                symbol_id: Some(claim.symbol_id.clone()),
                evidence_refs: vec!["write_claim".to_string()],
            });
            continue;
        }
        claims_by_symbol
            .entry(claim.symbol_id.as_str())
            .or_default()
            .insert(claim.task_id.clone());
    }
    for (symbol_id, tasks) in claims_by_symbol {
        if tasks.len() > 1 {
            conflicts.insert(HybridConflict {
                kind: HybridConflictKind::WriteWriteCollision,
                task_ids: tasks.into_iter().collect(),
                edge: None,
                symbol_id: Some(symbol_id.to_string()),
                evidence_refs: Vec::new(),
            });
        }
    }

    let mut proposed_hard_edges = Vec::new();
    for edge in &model_edges {
        let mechanical = candidate_kinds.get(edge);
        proposed_hard_edges.push(HybridHardEdge {
            edge: edge.clone(),
            provenance: if mechanical.is_some() {
                HybridEdgeProvenance::Both
            } else {
                HybridEdgeProvenance::Model
            },
            kinds: mechanical.into_iter().flatten().copied().collect(),
            evidence_refs: candidate_refs
                .get(edge)
                .into_iter()
                .flatten()
                .cloned()
                .collect(),
        });
    }
    for (from, to) in accepted_additions {
        let edge = HybridTaskEdge { from, to };
        proposed_hard_edges.push(HybridHardEdge {
            edge: edge.clone(),
            provenance: HybridEdgeProvenance::TrustedMechanical,
            kinds: candidate_kinds
                .get(&edge)
                .into_iter()
                .flatten()
                .copied()
                .collect(),
            evidence_refs: candidate_refs
                .get(&edge)
                .into_iter()
                .flatten()
                .cloned()
                .collect(),
        });
    }
    proposed_hard_edges.sort();

    let hard_pairs: BTreeSet<(String, String)> = proposed_hard_edges
        .iter()
        .map(|hard| (hard.edge.from.clone(), hard.edge.to.clone()))
        .collect();
    let acyclic = cycle_nodes(&declared, &hard_pairs).is_empty();
    let conflicts: Vec<_> = conflicts.into_iter().collect();
    let degradations: Vec<_> = degradations.into_iter().collect();
    // The adoption gate is a pure function of the three orthogonal safety
    // signals; the legacy bool is kept exactly in sync as its verdict.
    let adoption = HybridAdoptionDecision::from_gates(acyclic, conflicts.len(), degradations.len());
    let automatic_adoption_allowed = adoption.is_adopt();
    HybridDag {
        policy_version: HYBRID_DAG_POLICY_VERSION.to_string(),
        task_ids,
        model_edges,
        trusted_addition_candidates,
        weak_hints: weak_hints.into_iter().collect(),
        conflicts: conflicts.clone(),
        proposed_hard_edges,
        degradations: degradations.clone(),
        acyclic,
        automatic_adoption_allowed,
        adoption,
    }
}

/// Map an [`shuttle-symbols`] graph confidence onto the protocol mirror. 1:1.
fn lower_conf(c: GraphConfidence) -> OutcomeConfidence {
    match c {
        GraphConfidence::High => OutcomeConfidence::High,
        GraphConfidence::Medium => OutcomeConfidence::Medium,
        GraphConfidence::Low => OutcomeConfidence::Low,
    }
}

fn conf_word(c: OutcomeConfidence) -> &'static str {
    match c {
        OutcomeConfidence::High => "high",
        OutcomeConfidence::Medium => "medium",
        OutcomeConfidence::Low => "low",
    }
}

fn graph_kind_word(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::ImportDirected => "import_directed",
        EdgeKind::ContractDirected => "contract_directed",
        EdgeKind::NameMatch => "name_match",
        EdgeKind::CoChange => "co_change",
    }
}

fn symbol_kind_word(kind: &shuttle_symbols::SymbolKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "other".to_string())
}

fn lower_symbol(symbol: &shuttle_symbols::Symbol) -> DerivationSymbolRef {
    DerivationSymbolRef {
        name: symbol.name.clone(),
        path: norm(&symbol.path),
        kind: symbol_kind_word(&symbol.kind),
    }
}

fn is_acyclic(nodes: &[String], edges: &[DerivedTaskEdge]) -> bool {
    let mut indegree: BTreeMap<String, usize> = nodes.iter().map(|n| (n.clone(), 0)).collect();
    let mut outgoing: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for edge in edges {
        *indegree.entry(edge.to.clone()).or_default() += 1;
        outgoing
            .entry(edge.from.as_str())
            .or_default()
            .push(edge.to.as_str());
    }
    let mut ready: BTreeSet<String> = indegree
        .iter()
        .filter_map(|(node, degree)| (*degree == 0).then_some(node.clone()))
        .collect();
    let mut visited = 0usize;
    while let Some(node) = ready.pop_first() {
        visited += 1;
        if let Some(next) = outgoing.get(node.as_str()) {
            for dependent in next {
                let degree = indegree.get_mut(*dependent).expect("edge endpoint exists");
                *degree -= 1;
                if *degree == 0 {
                    ready.insert((*dependent).to_string());
                }
            }
        }
    }
    visited == indegree.len()
}

/// Derive advisory task dependencies and touch hints from verified write
/// evidence. Ambiguous/missing seeds, bounded closure truncation, or a cycle
/// set fallback_required; no planner edge is read or rewritten.
pub fn derive_edges(
    plan: &DerivePlan,
    graph: &SymbolGraph,
    table: &SymbolTable,
    index_status: &str,
    max_depth: usize,
    max_fanout: usize,
) -> SplitDerivationDetail {
    let mut definitions: BTreeMap<&str, Vec<&shuttle_symbols::Symbol>> = BTreeMap::new();
    for symbol in &table.symbols {
        definitions.entry(&symbol.name).or_default().push(symbol);
    }
    let mut symbol_owner: BTreeMap<String, String> = BTreeMap::new();
    let mut ambiguous_owned_names = BTreeSet::new();
    let mut task_writes: BTreeMap<String, BTreeSet<DerivationSymbolRef>> = BTreeMap::new();
    let mut ambiguous_input = false;
    let mut seen_task_ids = BTreeSet::new();
    for task in &plan.tasks {
        if !seen_task_ids.insert(task.id.clone()) {
            ambiguous_input = true;
        }
        let files: BTreeSet<String> = task.write_files.iter().map(|path| norm(path)).collect();
        let mut writes = BTreeSet::new();
        for symbol in &table.symbols {
            if path_owned_by(&symbol.path, &files) {
                writes.insert(lower_symbol(symbol));
            }
        }
        for seed in &task.write_symbols {
            let matches: Vec<&shuttle_symbols::Symbol> = definitions
                .get(seed.name.as_str())
                .into_iter()
                .flatten()
                .copied()
                .filter(|symbol| {
                    let expected = BTreeSet::from([norm(&seed.path)]);
                    path_owned_by(&symbol.path, &expected)
                })
                .collect();
            if matches.len() == 1 {
                writes.insert(lower_symbol(matches[0]));
            } else {
                ambiguous_input = true;
            }
        }
        if writes.is_empty() {
            ambiguous_input = true;
        }
        for symbol in &writes {
            if definitions
                .get(symbol.name.as_str())
                .is_none_or(|defs| defs.len() != 1)
            {
                ambiguous_input = true;
                ambiguous_owned_names.insert(symbol.name.clone());
                symbol_owner.remove(&symbol.name);
                continue;
            }
            if ambiguous_owned_names.contains(&symbol.name) {
                continue;
            }
            match symbol_owner.get(&symbol.name) {
                Some(previous) if previous != &task.id => {
                    ambiguous_input = true;
                    ambiguous_owned_names.insert(symbol.name.clone());
                    symbol_owner.remove(&symbol.name);
                }
                _ => {
                    symbol_owner.insert(symbol.name.clone(), task.id.clone());
                }
            }
        }
        task_writes.insert(task.id.clone(), writes);
    }

    let mut hard_edges = Vec::new();
    let mut weak_hints = Vec::new();
    let mut task_reads: BTreeMap<String, BTreeSet<DerivationSymbolRef>> = BTreeMap::new();
    for (owner, callee, confidence, kind) in graph.edges_with_kind() {
        let (Some(consumer), Some(provider)) = (symbol_owner.get(owner), symbol_owner.get(callee))
        else {
            continue;
        };
        if consumer == provider {
            continue;
        }
        let (Some(owner_defs), Some(callee_defs)) =
            (definitions.get(owner), definitions.get(callee))
        else {
            continue;
        };
        if owner_defs.len() != 1 || callee_defs.len() != 1 {
            ambiguous_input = true;
            continue;
        }
        let edge = DerivedTaskEdge {
            from: provider.clone(),
            to: consumer.clone(),
            from_sym: lower_symbol(callee_defs[0]),
            to_sym: lower_symbol(owner_defs[0]),
            confidence: lower_conf(confidence),
            kind: graph_kind_word(kind).to_string(),
        };
        task_reads
            .entry(consumer.clone())
            .or_default()
            .insert(edge.from_sym.clone());
        if kind.is_trusted_directed() {
            hard_edges.push(edge);
        } else {
            weak_hints.push(edge);
        }
    }
    hard_edges.sort_by(|a, b| {
        (&a.from, &a.to, &a.from_sym, &a.to_sym).cmp(&(&b.from, &b.to, &b.from_sym, &b.to_sym))
    });
    hard_edges.dedup();
    weak_hints.sort_by(|a, b| {
        (&a.from, &a.to, &a.from_sym, &a.to_sym).cmp(&(&b.from, &b.to, &b.from_sym, &b.to_sym))
    });
    weak_hints.dedup();

    let mut truncated = false;
    let mut truncation_reasons = BTreeSet::new();
    let mut tasks = Vec::new();
    for task in &plan.tasks {
        let writes = task_writes.get(&task.id).cloned().unwrap_or_default();
        let seeds: Vec<String> = writes
            .iter()
            .filter(|symbol| !ambiguous_owned_names.contains(&symbol.name))
            .map(|symbol| symbol.name.clone())
            .collect();
        let impact = graph.blast_radius_directed_bounded(
            &seeds,
            ClosureLimits {
                max_depth,
                max_fanout,
            },
        );
        truncated |= impact.truncated;
        truncation_reasons.extend(impact.truncation_reasons);
        let mut touch_hint: BTreeSet<String> =
            task.write_files.iter().map(|path| norm(path)).collect();
        touch_hint.extend(writes.iter().map(|symbol| symbol.path.clone()));
        touch_hint.extend(
            impact
                .hits
                .into_iter()
                .filter(|hit| !hit.path.is_empty())
                .map(|hit| norm(&hit.path)),
        );
        tasks.push(DerivedTask {
            id: task.id.clone(),
            write_symbols: writes.into_iter().collect(),
            read_symbols: task_reads
                .remove(&task.id)
                .unwrap_or_default()
                .into_iter()
                .collect(),
            touch_hint: touch_hint.into_iter().collect(),
        });
    }
    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    let node_ids: Vec<String> = tasks.iter().map(|task| task.id.clone()).collect();
    let acyclic = is_acyclic(&node_ids, &hard_edges);
    let fresh = index_status == "fresh";
    let fallback_required = !fresh || truncated || ambiguous_input || !acyclic;
    let degraded_reason = if !fresh {
        Some(format!("index-{index_status}"))
    } else if ambiguous_input {
        Some("ambiguous-write-evidence".to_string())
    } else if truncated {
        Some("closure-truncated".to_string())
    } else if !acyclic {
        Some("derived-cycle".to_string())
    } else {
        None
    };
    SplitDerivationDetail {
        goal: plan.goal.clone(),
        tasks,
        hard_edges,
        weak_hints,
        index_status: index_status.to_string(),
        max_depth,
        max_fanout,
        truncated,
        truncation_reasons: truncation_reasons.into_iter().collect(),
        acyclic,
        fallback_required,
        degraded_reason,
    }
}

// ---------------------------------------------------------------------------
// The adjudicator.
// ---------------------------------------------------------------------------

/// Adjudicate an intent DAG against the directional symbol graph, producing a
/// typed [`SplitDetail`] (the GoalGraph correction candidate).
///
/// Pure and deterministic: given the same `intent`, `graph`, and `table` it
/// always yields the same edges in the same order. Does no I/O.
pub fn correct_split(intent: &IntentDag, graph: &SymbolGraph, table: &SymbolTable) -> SplitDetail {
    // --- 0. task universe -------------------------------------------------
    let task_ids: Vec<String> = intent.tasks.iter().map(|t| t.id.clone()).collect();
    let id_set: BTreeSet<&str> = task_ids.iter().map(String::as_str).collect();

    // Per-task normalized file set.
    let mut task_files: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for t in &intent.tasks {
        let files: BTreeSet<String> = t.touched_files_guess.iter().map(|f| norm(f)).collect();
        task_files.insert(t.id.clone(), files);
    }

    // --- 1. symbol → owning tasks, and which tasks are actually observable
    //         in the scanned universe (have at least one def symbol). --------
    let mut sym_tasks: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    let mut task_in_universe: BTreeSet<String> = BTreeSet::new();
    for sym in &table.symbols {
        for (tid, files) in &task_files {
            if path_owned_by(&sym.path, files) {
                sym_tasks
                    .entry(sym.name.as_str())
                    .or_default()
                    .insert(tid.clone());
                task_in_universe.insert(tid.clone());
            }
        }
    }

    // --- 2. mechanical proven edges (from = prereq, to = dependent) --------
    // Only trusted directed edges are accepted for direction (T3). A symbol edge
    // owner→callee means "owner references (depends on) callee", so the callee's
    // task is the prerequisite and the owner's task is the dependent.
    let mut mech: BTreeMap<(String, String), GraphConfidence> = BTreeMap::new();
    for (owner, callee, conf, kind) in graph.edges_with_kind() {
        if !kind.is_trusted_directed() {
            continue; // weak / direction-unproven — proves nothing (§7)
        }
        let (Some(dep_tasks), Some(prereq_tasks)) = (sym_tasks.get(owner), sym_tasks.get(callee))
        else {
            continue;
        };
        for to in dep_tasks {
            for from in prereq_tasks {
                if to == from {
                    continue; // intra-task reference — not a task dependency
                }
                let key = (from.clone(), to.clone());
                // Keep the strongest corroboration seen. With High<Medium<Low,
                // "strongest" is the smallest variant.
                mech.entry(key)
                    .and_modify(|c| {
                        if conf < *c {
                            *c = conf;
                        }
                    })
                    .or_insert(conf);
            }
        }
    }
    let mech_keys: BTreeSet<(String, String)> = mech.keys().cloned().collect();
    let mech_star = reachable_pairs(&mech_keys, &task_ids);

    // --- 3. intent edges (from = prereq listed in depends_on_guess) --------
    // Track edges to known tasks separately from dangling ids (→ UNKNOWN).
    let mut intent_edges: BTreeSet<(String, String)> = BTreeSet::new();
    let mut dangling: Vec<(String, String)> = Vec::new(); // (from, to) with unknown `from`
    for t in &intent.tasks {
        for dep in &t.depends_on_guess {
            if dep == &t.id {
                continue; // self-dep, ignore
            }
            if id_set.contains(dep.as_str()) {
                intent_edges.insert((dep.clone(), t.id.clone()));
            } else {
                dangling.push((dep.clone(), t.id.clone()));
            }
        }
    }
    let intent_star = reachable_pairs(&intent_edges, &task_ids);

    // --- 4. adjudicate ----------------------------------------------------
    let mut edges: Vec<SplitEdge> = Vec::new();
    // Mech edges consumed while classifying intent edges (an agree or a
    // reverse-conflict) so they are not re-emitted from the mech-only pass.
    let mut mech_handled: BTreeSet<(String, String)> = BTreeSet::new();

    // 4a. every intent edge gets a verdict.
    for (from, to) in &intent_edges {
        let direct = (from.clone(), to.clone());
        let reverse = (to.clone(), from.clone());
        if let Some(c) = mech.get(&direct) {
            // Both sources agree on the same edge → AGREE. Two independent
            // corroborations lift confidence to High regardless of the raw
            // (per-edge, never-High) symbol confidence.
            mech_handled.insert(direct.clone());
            edges.push(SplitEdge {
                from: from.clone(),
                to: to.clone(),
                verdict: EdgeVerdict::Agree,
                confidence: OutcomeConfidence::High,
                source: EdgeSource::Both,
                evidence: format!(
                    "intent + trusted-directed symbol edge agree ({})",
                    conf_word(lower_conf(*c))
                ),
            });
        } else if mech.contains_key(&reverse) {
            // The graph proves the opposite direction directly → CONFLICT.
            mech_handled.insert(reverse.clone());
            edges.push(SplitEdge {
                from: from.clone(),
                to: to.clone(),
                verdict: EdgeVerdict::Conflict,
                confidence: OutcomeConfidence::Low,
                source: EdgeSource::Intent,
                evidence: format!(
                    "symbol graph proves the reverse direction ({to} depends on {from})"
                ),
            });
        } else if mech_star.contains(&reverse) {
            // Opposite order is forced transitively by the mechanical graph.
            edges.push(SplitEdge {
                from: from.clone(),
                to: to.clone(),
                verdict: EdgeVerdict::Conflict,
                confidence: OutcomeConfidence::Low,
                source: EdgeSource::Intent,
                evidence: format!(
                    "symbol graph transitively orders {to} before {from} (reverse of intent)"
                ),
            });
        } else if mech_star.contains(&direct) {
            // Same ordering is guaranteed transitively → the explicit edge is
            // redundant. Suggest dropping it, with the transitive evidence.
            edges.push(SplitEdge {
                from: from.clone(),
                to: to.clone(),
                verdict: EdgeVerdict::Drop,
                confidence: OutcomeConfidence::Medium,
                source: EdgeSource::Intent,
                evidence: "redundant: dependency already implied transitively by the symbol graph"
                    .to_string(),
            });
        } else {
            // Neither proven nor disproven → UNKNOWN (never guessed).
            let out_of_universe =
                !task_in_universe.contains(from) || !task_in_universe.contains(to);
            edges.push(SplitEdge {
                from: from.clone(),
                to: to.clone(),
                verdict: EdgeVerdict::Unknown,
                confidence: OutcomeConfidence::Low,
                source: EdgeSource::Intent,
                evidence: if out_of_universe {
                    "unprovable: task file(s) fall outside the scanned symbol universe".to_string()
                } else {
                    "unprovable: no import-directed corroboration in the scanned universe"
                        .to_string()
                },
            });
        }
    }

    // 4b. dangling intent edges (depends_on referencing an unknown task id).
    for (from, to) in &dangling {
        edges.push(SplitEdge {
            from: from.clone(),
            to: to.clone(),
            verdict: EdgeVerdict::Unknown,
            confidence: OutcomeConfidence::Low,
            source: EdgeSource::Intent,
            evidence: format!("depends_on id '{from}' is not among the intent tasks"),
        });
    }

    // 4c. mechanical edges the model omitted.
    for ((from, to), conf) in &mech {
        let direct = (from.clone(), to.clone());
        if mech_handled.contains(&direct) || intent_edges.contains(&direct) {
            continue; // already an AGREE/CONFLICT from the intent pass
        }
        if intent_star.contains(&direct) {
            continue; // ordering already implied by intent — no correction needed
        }
        if intent_star.contains(&(to.clone(), from.clone())) {
            // Intent forces the opposite order transitively → CONFLICT.
            edges.push(SplitEdge {
                from: from.clone(),
                to: to.clone(),
                verdict: EdgeVerdict::Conflict,
                confidence: OutcomeConfidence::Low,
                source: EdgeSource::Symbolgraph,
                evidence: format!(
                    "symbol graph requires {to} to depend on {from}, but intent orders them oppositely"
                ),
            });
        } else {
            // A genuine omission the graph proves → ADD.
            edges.push(SplitEdge {
                from: from.clone(),
                to: to.clone(),
                verdict: EdgeVerdict::Add,
                confidence: lower_conf(*conf),
                source: EdgeSource::Symbolgraph,
                evidence: format!(
                    "import-directed symbol edge proves {to} depends on {from} ({})",
                    conf_word(lower_conf(*conf))
                ),
            });
        }
    }

    // --- 5. deterministic order + tallies --------------------------------
    edges.sort_by(|a, b| {
        (a.from.as_str(), a.to.as_str(), a.verdict.as_str()).cmp(&(
            b.from.as_str(),
            b.to.as_str(),
            b.verdict.as_str(),
        ))
    });

    let mut added = 0usize;
    let mut dropped = 0usize;
    let mut review_required: Vec<usize> = Vec::new();
    for (i, e) in edges.iter().enumerate() {
        match e.verdict {
            EdgeVerdict::Add => added += 1,
            EdgeVerdict::Drop => dropped += 1,
            EdgeVerdict::Conflict | EdgeVerdict::Unknown => review_required.push(i),
            EdgeVerdict::Agree => {}
        }
    }
    let review_surface = review_required.len();

    let nodes: Vec<SplitNode> = intent
        .tasks
        .iter()
        .map(|t| SplitNode {
            id: t.id.clone(),
            intent: t.intent.clone(),
            touched_files: t.touched_files_guess.iter().map(|f| norm(f)).collect(),
        })
        .collect();

    SplitDetail {
        goal: intent.goal.clone(),
        nodes,
        edges,
        added,
        dropped,
        review_surface,
        review_required,
    }
}

// ---------------------------------------------------------------------------
// Lowering to the universal ToolOutcome envelope.
// ---------------------------------------------------------------------------

/// Lower a [`SplitDetail`] into a [`ToolOutcome`].
///
/// * Any CONFLICT/UNKNOWN edge → `Blocked` (`needs-review`): the one next action
///   is to adjudicate exactly those edges (review surface = O(disagreements)).
/// * Otherwise → `Green`: every edge was mechanically decided (AGREE/ADD/DROP).
pub fn split_outcome(detail: SplitDetail) -> ToolOutcome {
    let SplitDetail {
        added,
        dropped,
        review_surface,
        ..
    } = detail;

    if review_surface > 0 {
        let missing: Vec<String> = detail
            .review_required
            .iter()
            .filter_map(|&i| detail.edges.get(i))
            .map(|e| format!("{} → {} ({})", e.from, e.to, e.verdict.as_str()))
            .collect();
        let summary = format!(
            "split correction: +{added} add / -{dropped} drop / {review_surface} edge(s) need review"
        );
        ToolOutcome::blocked(
            TOOL_SPLIT,
            reason::NEEDS_REVIEW,
            "adjudicate the conflicting / unprovable edges before executing the DAG",
            TypedDetail::Split(detail),
        )
        .with_missing(missing)
        .with_summary(summary)
    } else {
        let summary =
            format!("split correction complete: +{added} add / -{dropped} drop / 0 review");
        ToolOutcome::green(TOOL_SPLIT, TypedDetail::Split(detail)).with_summary(summary)
    }
}

// ---------------------------------------------------------------------------
// I/O wrapper (the shape the CLI speaks).
// ---------------------------------------------------------------------------

/// Read an intent DAG from `plan_path`, build the directional symbol graph over
/// `opts.root`, adjudicate, and return the lowered [`ToolOutcome`].
///
/// A missing/unreadable plan or malformed JSON becomes an `Error` outcome
/// (`invalid-input`) rather than an `Err`, so the CLI always has a machine
/// envelope to print. A genuine scan failure propagates as [`NavError`].
pub fn split_correct(
    plan_path: &str,
    opts: &crate::QueryOptions,
) -> Result<ToolOutcome, crate::NavError> {
    let raw = match std::fs::read_to_string(plan_path) {
        Ok(s) => s,
        Err(e) => {
            return Ok(ToolOutcome::error(
                TOOL_SPLIT,
                reason::IO,
                format!("cannot read plan '{plan_path}': {e}"),
            ));
        }
    };
    let intent: IntentDag = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            return Ok(ToolOutcome::error(
                TOOL_SPLIT,
                reason::INVALID_INPUT,
                format!("plan '{plan_path}' is not a valid intent DAG: {e}"),
            ));
        }
    };
    adjudicate_intent(&intent, opts)
}

/// Read a versioned derive plan and a caller-owned symbol artifact, then emit
/// advisory edges/touch hints. The plan is never opened for writing.
pub fn split_derive_edges(
    plan_path: &str,
    root: &str,
    symbol_index: &str,
    index_head: &str,
    max_depth: usize,
    max_fanout: usize,
) -> Result<SplitDerivationDetail, crate::NavError> {
    let raw = std::fs::read_to_string(plan_path)?;
    let plan: DerivePlan = serde_json::from_str(&raw)?;
    let unsupported = plan.tasks.iter().any(|task| {
        task.write_files
            .iter()
            .map(String::as_str)
            .chain(task.write_symbols.iter().map(|seed| seed.path.as_str()))
            .any(|path| !shuttle_symbols::imports::supports_dependency_path(path))
    });
    if unsupported {
        return Ok(degraded_derivation(&plan, "absent", max_depth, max_fanout));
    }
    let (table, status) =
        match shuttle_symbols::index::read_artifact_for_revision(root, symbol_index, index_head) {
            shuttle_symbols::index::ArtifactRead::Fresh(table) => (table, "fresh"),
            shuttle_symbols::index::ArtifactRead::Stale { .. } => {
                return Ok(degraded_derivation(&plan, "stale", max_depth, max_fanout))
            }
            shuttle_symbols::index::ArtifactRead::Absent => {
                return Ok(degraded_derivation(&plan, "absent", max_depth, max_fanout))
            }
        };
    let graph = SymbolGraph::try_from_scan_root(&table, root)?;
    Ok(derive_edges(
        &plan, &graph, &table, status, max_depth, max_fanout,
    ))
}

/// Adjudicate an already-parsed [`IntentDag`] against the directional symbol
/// graph over `opts.root`, returning the lowered [`ToolOutcome`].
///
/// This is the in-memory entry the GoalGraph outer loop (`shuttle goal`, see
/// [`crate::goal`]) drives its adjudicator seam through — no plan-file round
/// trip. [`split_correct`] is the file-path wrapper that reads/parses an
/// `intent/v1` JSON and then calls this. A genuine scan failure propagates as
/// [`NavError`]; a clean adjudication (even one needing review) is an `Ok`
/// envelope, never an `Err`.
pub fn adjudicate_intent(
    intent: &IntentDag,
    opts: &crate::QueryOptions,
) -> Result<ToolOutcome, crate::NavError> {
    let table = crate::full_table(opts)?;
    let graph = SymbolGraph::try_from_scan(&table)?;
    let detail = correct_split(intent, &graph, &table);
    Ok(split_outcome(detail))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn ctags_present() -> bool {
        SymbolTable::scan(".").is_ok()
    }

    fn scratch(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("shuttle_split_{tag}_{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, body: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p.to_string_lossy().to_string()
    }

    fn scan(dir: &Path) -> (SymbolGraph, SymbolTable) {
        let table = SymbolTable::scan(&dir.to_string_lossy()).unwrap();
        let graph = SymbolGraph::from_scan(&table);
        (graph, table)
    }

    /// Two Python files where `wrapper` imports and calls into `core`; the
    /// symbol graph therefore proves wrapper depends on core.
    fn wrapper_core(dir: &Path) -> (String, String) {
        let core = write(
            dir,
            "core.py",
            "def core_fn():\n    return 1\n\nclass Core:\n    pass\n",
        );
        let wrapper = write(
            dir,
            "wrapper.py",
            "import core\n\ndef wrap():\n    return core.core_fn()\n",
        );
        (core, wrapper)
    }

    fn find<'a>(d: &'a SplitDetail, from: &str, to: &str) -> Option<&'a SplitEdge> {
        d.edges.iter().find(|e| e.from == from && e.to == to)
    }

    #[test]
    fn agree_when_intent_matches_import_direction() {
        if !ctags_present() {
            return;
        }
        let dir = scratch("agree");
        let (core, wrapper) = wrapper_core(&dir);
        let (g, t) = scan(&dir);
        let intent = IntentDag {
            goal: "demo".into(),
            version: INTENT_VERSION.into(),
            tasks: vec![
                IntentTask {
                    id: "core".into(),
                    intent: "core".into(),
                    touched_files_guess: vec![core],
                    depends_on_guess: vec![],
                    acceptance: None,
                    invariants: None,
                },
                IntentTask {
                    id: "wrapper".into(),
                    intent: "wrapper".into(),
                    touched_files_guess: vec![wrapper],
                    depends_on_guess: vec!["core".into()],
                    acceptance: None,
                    invariants: None,
                },
            ],
        };
        let d = correct_split(&intent, &g, &t);
        let e = find(&d, "core", "wrapper").expect("edge present");
        assert_eq!(e.verdict, EdgeVerdict::Agree, "detail: {d:?}");
        assert_eq!(e.confidence, OutcomeConfidence::High);
        assert_eq!(e.source, EdgeSource::Both);
        assert_eq!(d.review_surface, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_when_model_omits_a_proven_dependency() {
        if !ctags_present() {
            return;
        }
        let dir = scratch("add");
        let (core, wrapper) = wrapper_core(&dir);
        let (g, t) = scan(&dir);
        let intent = IntentDag {
            goal: "demo".into(),
            version: INTENT_VERSION.into(),
            tasks: vec![
                IntentTask {
                    id: "core".into(),
                    intent: String::new(),
                    touched_files_guess: vec![core],
                    depends_on_guess: vec![],
                    acceptance: None,
                    invariants: None,
                },
                IntentTask {
                    id: "wrapper".into(),
                    intent: String::new(),
                    touched_files_guess: vec![wrapper],
                    depends_on_guess: vec![], // omitted the real dependency
                    acceptance: None,
                    invariants: None,
                },
            ],
        };
        let d = correct_split(&intent, &g, &t);
        let e = find(&d, "core", "wrapper").expect("added edge present");
        assert_eq!(e.verdict, EdgeVerdict::Add, "detail: {d:?}");
        assert_eq!(e.source, EdgeSource::Symbolgraph);
        assert_eq!(d.added, 1);
        assert_eq!(d.review_surface, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn conflict_when_intent_reverses_proven_direction() {
        if !ctags_present() {
            return;
        }
        let dir = scratch("conflict");
        let (core, wrapper) = wrapper_core(&dir);
        let (g, t) = scan(&dir);
        let intent = IntentDag {
            goal: "demo".into(),
            version: INTENT_VERSION.into(),
            tasks: vec![
                IntentTask {
                    id: "core".into(),
                    intent: String::new(),
                    touched_files_guess: vec![core],
                    depends_on_guess: vec!["wrapper".into()], // reversed!
                    acceptance: None,
                    invariants: None,
                },
                IntentTask {
                    id: "wrapper".into(),
                    intent: String::new(),
                    touched_files_guess: vec![wrapper],
                    depends_on_guess: vec![],
                    acceptance: None,
                    invariants: None,
                },
            ],
        };
        let d = correct_split(&intent, &g, &t);
        let e = find(&d, "wrapper", "core").expect("reversed intent edge present");
        assert_eq!(e.verdict, EdgeVerdict::Conflict, "detail: {d:?}");
        assert!(d.review_surface >= 1);
        assert!(d
            .review_required
            .contains(&d.edges.iter().position(|x| std::ptr::eq(x, e)).unwrap()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_when_no_mechanical_corroboration() {
        if !ctags_present() {
            return;
        }
        let dir = scratch("unknown");
        let a = write(dir.as_path(), "a.py", "def a_fn():\n    return 1\n");
        let b = write(dir.as_path(), "b.py", "def b_fn():\n    return 2\n");
        let (g, t) = scan(&dir);
        // No import between a and b, so no import-directed edge exists.
        let intent = IntentDag {
            goal: "demo".into(),
            version: INTENT_VERSION.into(),
            tasks: vec![
                IntentTask {
                    id: "a".into(),
                    intent: String::new(),
                    touched_files_guess: vec![a],
                    depends_on_guess: vec![],
                    acceptance: None,
                    invariants: None,
                },
                IntentTask {
                    id: "b".into(),
                    intent: String::new(),
                    touched_files_guess: vec![b],
                    depends_on_guess: vec!["a".into()], // unprovable claim
                    acceptance: None,
                    invariants: None,
                },
            ],
        };
        let d = correct_split(&intent, &g, &t);
        let e = find(&d, "a", "b").expect("unprovable edge present");
        assert_eq!(e.verdict, EdgeVerdict::Unknown, "detail: {d:?}");
        assert_eq!(d.review_surface, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn drop_when_direct_edge_is_transitively_redundant() {
        if !ctags_present() {
            return;
        }
        let dir = scratch("drop");
        let base = write(dir.as_path(), "base.py", "def base_fn():\n    return 0\n");
        let mid = write(
            dir.as_path(),
            "mid.py",
            "import base\n\ndef mid_fn():\n    return base.base_fn()\n",
        );
        let top = write(
            dir.as_path(),
            "top.py",
            "import mid\n\ndef top_fn():\n    return mid.mid_fn()\n",
        );
        let (g, t) = scan(&dir);
        let intent = IntentDag {
            goal: "demo".into(),
            version: INTENT_VERSION.into(),
            tasks: vec![
                IntentTask {
                    id: "base".into(),
                    intent: String::new(),
                    touched_files_guess: vec![base],
                    depends_on_guess: vec![],
                    acceptance: None,
                    invariants: None,
                },
                IntentTask {
                    id: "mid".into(),
                    intent: String::new(),
                    touched_files_guess: vec![mid],
                    depends_on_guess: vec!["base".into()],
                    acceptance: None,
                    invariants: None,
                },
                IntentTask {
                    id: "top".into(),
                    intent: String::new(),
                    touched_files_guess: vec![top],
                    depends_on_guess: vec!["mid".into(), "base".into()], // base is redundant
                    acceptance: None,
                    invariants: None,
                },
            ],
        };
        let d = correct_split(&intent, &g, &t);
        let e = find(&d, "base", "top").expect("redundant edge present");
        assert_eq!(e.verdict, EdgeVerdict::Drop, "detail: {d:?}");
        assert_eq!(d.dropped, 1);
        // The chain edges themselves agree.
        assert_eq!(find(&d, "base", "mid").unwrap().verdict, EdgeVerdict::Agree);
        assert_eq!(find(&d, "mid", "top").unwrap().verdict, EdgeVerdict::Agree);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn outcome_is_green_when_all_decided_and_blocked_on_review() {
        if !ctags_present() {
            return;
        }
        let dir = scratch("outcome");
        let (core, wrapper) = wrapper_core(&dir);
        let (g, t) = scan(&dir);
        // All-agree DAG → Green.
        let good = IntentDag {
            goal: "g".into(),
            version: INTENT_VERSION.into(),
            tasks: vec![
                IntentTask {
                    id: "core".into(),
                    intent: String::new(),
                    touched_files_guess: vec![core.clone()],
                    depends_on_guess: vec![],
                    acceptance: None,
                    invariants: None,
                },
                IntentTask {
                    id: "wrapper".into(),
                    intent: String::new(),
                    touched_files_guess: vec![wrapper.clone()],
                    depends_on_guess: vec!["core".into()],
                    acceptance: None,
                    invariants: None,
                },
            ],
        };
        let o = split_outcome(correct_split(&good, &g, &t));
        assert_eq!(o.state, shuttle_protocol::OutcomeState::Green);
        assert!(!o.state.is_error());

        // Reversed DAG → Blocked/needs-review.
        let bad = IntentDag {
            goal: "g".into(),
            version: INTENT_VERSION.into(),
            tasks: vec![
                IntentTask {
                    id: "core".into(),
                    intent: String::new(),
                    touched_files_guess: vec![core],
                    depends_on_guess: vec!["wrapper".into()],
                    acceptance: None,
                    invariants: None,
                },
                IntentTask {
                    id: "wrapper".into(),
                    intent: String::new(),
                    touched_files_guess: vec![wrapper],
                    depends_on_guess: vec![],
                    acceptance: None,
                    invariants: None,
                },
            ],
        };
        let o = split_outcome(correct_split(&bad, &g, &t));
        assert_eq!(o.state, shuttle_protocol::OutcomeState::Blocked);
        assert_eq!(o.reason_code, reason::NEEDS_REVIEW);
        assert!(o.next_action.is_some());
        assert!(!o.whats_missing.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn io_wrapper_reads_plan_json_end_to_end() {
        if !ctags_present() {
            return;
        }
        let dir = scratch("io");
        let (core, wrapper) = wrapper_core(&dir);
        let plan = IntentDag {
            goal: "e2e".into(),
            version: INTENT_VERSION.into(),
            tasks: vec![
                IntentTask {
                    id: "core".into(),
                    intent: "core".into(),
                    touched_files_guess: vec![core],
                    depends_on_guess: vec![],
                    acceptance: None,
                    invariants: None,
                },
                IntentTask {
                    id: "wrapper".into(),
                    intent: "wrapper".into(),
                    touched_files_guess: vec![wrapper],
                    depends_on_guess: vec!["core".into()],
                    acceptance: None,
                    invariants: None,
                },
            ],
        };
        let plan_path = dir.join("plan.json");
        std::fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();
        let mut opts = crate::QueryOptions::new(dir.to_string_lossy().to_string());
        opts.cache = false;
        let o = split_correct(&plan_path.to_string_lossy(), &opts).unwrap();
        assert_eq!(o.state, shuttle_protocol::OutcomeState::Green);
        match o.detail {
            TypedDetail::Split(d) => {
                assert_eq!(d.goal, "e2e");
                assert_eq!(d.nodes.len(), 2);
                assert_eq!(
                    find(&d, "core", "wrapper").unwrap().verdict,
                    EdgeVerdict::Agree
                );
            }
            _ => panic!("expected split detail"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn io_wrapper_reports_bad_input_as_error_outcome() {
        let dir = scratch("badinput");
        let plan_path = dir.join("plan.json");
        std::fs::write(&plan_path, "{ not valid json").unwrap();
        let opts = crate::QueryOptions::new(dir.to_string_lossy().to_string());
        let o = split_correct(&plan_path.to_string_lossy(), &opts).unwrap();
        assert!(o.state.is_error());
        assert_eq!(o.reason_code, reason::INVALID_INPUT);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_plan_file_is_an_io_error_outcome() {
        let opts = crate::QueryOptions::new(".".to_string());
        let o = split_correct("/nonexistent/does/not/exist/plan.json", &opts).unwrap();
        assert!(o.state.is_error());
        assert_eq!(o.reason_code, reason::IO);
    }

    // -----------------------------------------------------------------------
    // intent/v2 backward-compat (§4.1): the new optional acceptance/invariants
    // fields never break v1 parsing, round-trip losslessly, and are invisible
    // to the adjudicator (proving dependencies never consults acceptance text).
    // -----------------------------------------------------------------------

    #[test]
    fn v1_intent_without_new_fields_still_deserializes() {
        // A verbatim intent/v1 JSON: no acceptance, no invariants, v1 version.
        let raw = r#"{
            "goal": "demo",
            "version": "intent/v1",
            "tasks": [
                { "id": "a", "intent": "edit a", "touched_files_guess": ["a.py"], "depends_on_guess": [] },
                { "id": "b", "intent": "edit b", "touched_files_guess": ["b.py"], "depends_on_guess": ["a"] }
            ]
        }"#;
        let dag: IntentDag = serde_json::from_str(raw).expect("v1 must still parse");
        assert_eq!(dag.version, INTENT_VERSION_V1);
        assert_eq!(dag.tasks.len(), 2);
        // #[serde(default)] fills the absent optionals with None — never fabricated.
        assert!(dag
            .tasks
            .iter()
            .all(|t| t.acceptance.is_none() && t.invariants.is_none()));
    }

    #[test]
    fn v2_intent_roundtrips_through_serde() {
        let dag = IntentDag {
            goal: "demo".into(),
            version: INTENT_VERSION.into(),
            tasks: vec![IntentTask {
                id: "a".into(),
                intent: "edit a".into(),
                touched_files_guess: vec!["a.py".into()],
                depends_on_guess: vec![],
                acceptance: Some(vec!["returns 200 on happy path".into()]),
                invariants: Some(vec!["never panics".into()]),
            }],
        };
        let json = serde_json::to_string(&dag).unwrap();
        // The optionals must serialize (they are Some here).
        assert!(
            json.contains("acceptance"),
            "acceptance must serialize: {json}"
        );
        assert!(
            json.contains("invariants"),
            "invariants must serialize: {json}"
        );
        let back: IntentDag = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, INTENT_VERSION);
        assert_eq!(
            back.tasks[0].acceptance.as_deref(),
            Some(&["returns 200 on happy path".to_string()][..])
        );
        assert_eq!(
            back.tasks[0].invariants.as_deref(),
            Some(&["never panics".to_string()][..])
        );
    }

    #[test]
    fn none_optionals_are_omitted_from_serialized_json() {
        // skip_serializing_if keeps a v1-shaped task byte-clean (no null noise) —
        // an shuttle-emitted DAG whose tasks named no acceptance stays v1-compatible.
        let dag = IntentDag {
            goal: "demo".into(),
            version: INTENT_VERSION.into(),
            tasks: vec![IntentTask {
                id: "a".into(),
                intent: "edit a".into(),
                touched_files_guess: vec!["a.py".into()],
                depends_on_guess: vec![],
                acceptance: None,
                invariants: None,
            }],
        };
        let json = serde_json::to_string(&dag).unwrap();
        assert!(
            !json.contains("acceptance"),
            "None acceptance must be omitted: {json}"
        );
        assert!(
            !json.contains("invariants"),
            "None invariants must be omitted: {json}"
        );
    }

    #[test]
    fn adjudicator_ignores_acceptance_and_invariants() {
        // Two DAGs identical except one carries acceptance/invariants text. The
        // adjudicator proves the same edge verdicts either way — acceptance is a
        // reconcile-time concern, never a dependency-proving input.
        if !ctags_present() {
            return;
        }
        let dir = scratch("v2ignore");
        let (core, wrapper) = wrapper_core(&dir);
        let opts = crate::QueryOptions::new(dir.to_string_lossy().to_string());

        let plain = IntentDag {
            goal: "demo".into(),
            version: INTENT_VERSION.into(),
            tasks: vec![
                IntentTask {
                    id: "core".into(),
                    intent: String::new(),
                    touched_files_guess: vec![core.clone()],
                    depends_on_guess: vec![],
                    acceptance: None,
                    invariants: None,
                },
                IntentTask {
                    id: "wrapper".into(),
                    intent: String::new(),
                    touched_files_guess: vec![wrapper.clone()],
                    depends_on_guess: vec!["core".into()],
                    acceptance: None,
                    invariants: None,
                },
            ],
        };
        let mut annotated = plain.clone();
        annotated.tasks[0].acceptance = Some(vec!["core_fn returns 1".into()]);
        annotated.tasks[1].invariants = Some(vec!["wrap stays pure".into()]);

        let o_plain = adjudicate_intent(&plain, &opts).unwrap();
        let o_annot = adjudicate_intent(&annotated, &opts).unwrap();
        // Same state, same reason — the extra text changed nothing mechanically.
        assert_eq!(o_plain.state, o_annot.state);
        assert_eq!(o_plain.reason_code, o_annot.reason_code);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn derive_symbol(name: &str, path: &str) -> shuttle_symbols::Symbol {
        shuttle_symbols::Symbol {
            name: name.to_string(),
            kind: shuttle_symbols::SymbolKind::Function,
            scope: None,
            path: path.to_string(),
            line: 1,
            signature: Some(format!("def {name}()")),
        }
    }

    fn derive_task(id: &str, name: &str, path: &str) -> DeriveTaskInput {
        DeriveTaskInput {
            id: id.to_string(),
            intent: String::new(),
            write_files: Vec::new(),
            write_symbols: vec![DeriveSymbolSeed {
                name: name.to_string(),
                path: path.to_string(),
            }],
        }
    }

    #[test]
    fn derive_edges_emits_trusted_task_edges_and_exact_reverse_closure() {
        let table = SymbolTable {
            symbols: vec![
                derive_symbol("core", "a.py"),
                derive_symbol("wrapper", "b.py"),
                derive_symbol("check", "c.py"),
            ],
        };
        let contents = BTreeMap::from([
            ("a.py".to_string(), "def core(): return 1\n".to_string()),
            (
                "b.py".to_string(),
                "from a import core\ndef wrapper(): return core()\n".to_string(),
            ),
            (
                "c.py".to_string(),
                "from b import wrapper\ndef check(): return wrapper()\n".to_string(),
            ),
        ]);
        let graph = SymbolGraph::build(&table, &contents);
        let plan = DerivePlan {
            goal: "chain".to_string(),
            version: "split-derive/v1".to_string(),
            tasks: vec![
                derive_task("a", "core", "a.py"),
                derive_task("b", "wrapper", "b.py"),
                derive_task("c", "check", "c.py"),
            ],
        };
        let detail = derive_edges(&plan, &graph, &table, "fresh", 8, 8);
        assert_eq!(
            detail
                .hard_edges
                .iter()
                .map(|edge| (edge.from.as_str(), edge.to.as_str()))
                .collect::<Vec<_>>(),
            vec![("a", "b"), ("b", "c")]
        );
        let a = detail.tasks.iter().find(|task| task.id == "a").unwrap();
        assert_eq!(a.touch_hint, vec!["a.py", "b.py", "c.py"]);
        assert!(!detail.truncated);
        assert!(!detail.fallback_required);
        assert!(detail.acyclic);
    }

    #[test]
    fn derive_edges_accepts_contract_direction_and_rejects_weak_serialization() {
        let table = SymbolTable {
            symbols: vec![
                derive_symbol("Provider", "provider.go"),
                derive_symbol("Consumer", "consumer.ts"),
                derive_symbol("Coincidence", "coincidence.py"),
            ],
        };
        let mut graph = SymbolGraph::build(&table, &BTreeMap::new());
        graph.add_contract_edge("Consumer", "Provider");
        graph.add_cochange_edge("Coincidence", "Provider");
        let plan = DerivePlan {
            goal: "mixed evidence".to_string(),
            version: "split-derive/v1".to_string(),
            tasks: vec![
                derive_task("provider", "Provider", "provider.go"),
                derive_task("consumer", "Consumer", "consumer.ts"),
                derive_task("coincidence", "Coincidence", "coincidence.py"),
            ],
        };
        let detail = derive_edges(&plan, &graph, &table, "fresh", 8, 8);
        assert_eq!(detail.hard_edges.len(), 1);
        assert_eq!(detail.hard_edges[0].from, "provider");
        assert_eq!(detail.hard_edges[0].to, "consumer");
        assert_eq!(detail.hard_edges[0].kind, "contract_directed");
        assert_eq!(detail.weak_hints.len(), 1);
        assert_eq!(detail.weak_hints[0].to, "coincidence");
        assert_eq!(detail.weak_hints[0].kind, "co_change");
    }

    #[test]
    fn derive_edges_marks_bounds_and_is_deterministic() {
        let table = SymbolTable {
            symbols: vec![
                derive_symbol("seed", "seed.py"),
                derive_symbol("left", "left.py"),
                derive_symbol("right", "right.py"),
            ],
        };
        let contents = BTreeMap::from([
            ("seed.py".to_string(), "def seed(): return 1\n".to_string()),
            (
                "left.py".to_string(),
                "from seed import seed\ndef left(): return seed()\n".to_string(),
            ),
            (
                "right.py".to_string(),
                "from seed import seed\ndef right(): return seed()\n".to_string(),
            ),
        ]);
        let graph = SymbolGraph::build(&table, &contents);
        let plan = DerivePlan {
            goal: "bounded".to_string(),
            version: "split-derive/v1".to_string(),
            tasks: vec![derive_task("seed", "seed", "seed.py")],
        };
        let first = derive_edges(&plan, &graph, &table, "fresh", 8, 1);
        let second = derive_edges(&plan, &graph, &table, "fresh", 8, 1);
        assert_eq!(first, second);
        assert!(first.truncated);
        assert_eq!(first.truncation_reasons, vec!["max_fanout"]);
        assert!(first.fallback_required);
        assert_eq!(first.degraded_reason.as_deref(), Some("closure-truncated"));
        assert_eq!(first.tasks[0].touch_hint.len(), 2);
    }

    #[test]
    fn degraded_derivation_preserves_task_ids_and_explicit_file_fallback() {
        let plan = DerivePlan {
            goal: "degrade".to_string(),
            version: "split-derive/v1".to_string(),
            tasks: vec![DeriveTaskInput {
                id: "task-b".to_string(),
                intent: String::new(),
                write_files: vec!["./b.py".to_string(), "b.py".to_string()],
                write_symbols: vec![DeriveSymbolSeed {
                    name: "b".to_string(),
                    path: "b.py".to_string(),
                }],
            }],
        };
        let detail = degraded_derivation(&plan, "stale", 4, 16);
        assert_eq!(detail.index_status, "stale");
        assert_eq!(detail.tasks.len(), 1);
        assert_eq!(detail.tasks[0].id, "task-b");
        assert_eq!(detail.tasks[0].touch_hint, vec!["b.py"]);
        assert!(detail.tasks[0].write_symbols.is_empty());
        assert!(detail.hard_edges.is_empty());
        assert!(detail.fallback_required);
    }

    #[test]
    fn split_correction_treats_contract_as_trusted_and_weak_as_unknown() {
        let table = SymbolTable {
            symbols: vec![
                derive_symbol("Provider", "provider.go"),
                derive_symbol("Consumer", "consumer.ts"),
                derive_symbol("Weak", "weak.py"),
            ],
        };
        let mut graph = SymbolGraph::build(&table, &BTreeMap::new());
        graph.add_contract_edge("Consumer", "Provider");
        graph.add_cochange_edge("Weak", "Provider");
        let intent = IntentDag {
            goal: "correct trusted kinds".to_string(),
            version: INTENT_VERSION.into(),
            tasks: vec![
                IntentTask {
                    id: "provider".to_string(),
                    intent: String::new(),
                    touched_files_guess: vec!["provider.go".to_string()],
                    depends_on_guess: Vec::new(),
                    acceptance: None,
                    invariants: None,
                },
                IntentTask {
                    id: "consumer".to_string(),
                    intent: String::new(),
                    touched_files_guess: vec!["consumer.ts".to_string()],
                    depends_on_guess: vec!["provider".to_string()],
                    acceptance: None,
                    invariants: None,
                },
                IntentTask {
                    id: "weak".to_string(),
                    intent: String::new(),
                    touched_files_guess: vec!["weak.py".to_string()],
                    depends_on_guess: vec!["provider".to_string()],
                    acceptance: None,
                    invariants: None,
                },
            ],
        };
        let detail = correct_split(&intent, &graph, &table);
        assert_eq!(
            find(&detail, "provider", "consumer").unwrap().verdict,
            EdgeVerdict::Agree
        );
        assert_eq!(
            find(&detail, "provider", "weak").unwrap().verdict,
            EdgeVerdict::Unknown
        );
    }
    fn hybrid_evidence(
        owner: &str,
        callee: &str,
        kind: HybridEvidenceKind,
        state: HybridEvidenceState,
        evidence_ref: &str,
    ) -> HybridMechanicalEvidence {
        HybridMechanicalEvidence {
            owner_task: owner.into(),
            callee_task: callee.into(),
            kind,
            state,
            evidence_ref: evidence_ref.into(),
        }
    }

    #[test]
    fn hybrid_merge_is_monotonic_trusted_and_deterministic() {
        let input = HybridMergeInput {
            task_ids: vec!["review".into(), "consumer".into(), "provider".into()],
            model_edges: vec![HybridTaskEdge {
                from: "provider".into(),
                to: "consumer".into(),
            }],
            mechanical_edges: vec![
                hybrid_evidence(
                    "consumer",
                    "provider",
                    HybridEvidenceKind::ImportDirected,
                    HybridEvidenceState::FreshExactUnique,
                    "index://import",
                ),
                hybrid_evidence(
                    "review",
                    "provider",
                    HybridEvidenceKind::ContractDirected,
                    HybridEvidenceState::FreshExactUnique,
                    "index://contract",
                ),
                hybrid_evidence(
                    "review",
                    "consumer",
                    HybridEvidenceKind::NameMatch,
                    HybridEvidenceState::FreshExactUnique,
                    "index://name",
                ),
                hybrid_evidence(
                    "consumer",
                    "review",
                    HybridEvidenceKind::CoChange,
                    HybridEvidenceState::FreshExactUnique,
                    "history://cochange",
                ),
            ],
            write_claims: Vec::new(),
            evidence_issues: Vec::new(),
        };
        let merged = merge_hybrid_dag(&input);
        assert!(merged.automatic_adoption_allowed);
        assert_eq!(merged.proposed_hard_edges.len(), 2);
        assert!(merged
            .proposed_hard_edges
            .iter()
            .any(|hard| hard.edge == input.model_edges[0]
                && hard.provenance == HybridEdgeProvenance::Both));
        assert!(merged
            .proposed_hard_edges
            .iter()
            .any(|hard| hard.edge.from == "provider"
                && hard.edge.to == "review"
                && hard.provenance == HybridEdgeProvenance::TrustedMechanical));
        assert_eq!(merged.weak_hints.len(), 2);
        assert!(merged
            .proposed_hard_edges
            .iter()
            .all(|hard| hard.kinds.iter().all(|kind| kind.is_trusted_directed())));
        let mut reordered = input;
        reordered.task_ids.reverse();
        reordered.mechanical_edges.reverse();
        assert_eq!(merged, merge_hybrid_dag(&reordered));
    }

    #[test]
    fn hybrid_merge_reports_reverse_and_candidate_cycles() {
        let input = HybridMergeInput {
            task_ids: vec!["a".into(), "b".into(), "c".into()],
            model_edges: vec![HybridTaskEdge {
                from: "a".into(),
                to: "b".into(),
            }],
            mechanical_edges: vec![
                hybrid_evidence(
                    "a",
                    "b",
                    HybridEvidenceKind::ImportDirected,
                    HybridEvidenceState::FreshExactUnique,
                    "index://reverse",
                ),
                hybrid_evidence(
                    "c",
                    "b",
                    HybridEvidenceKind::ContractDirected,
                    HybridEvidenceState::FreshExactUnique,
                    "index://b-to-c",
                ),
                hybrid_evidence(
                    "b",
                    "c",
                    HybridEvidenceKind::ImportDirected,
                    HybridEvidenceState::FreshExactUnique,
                    "index://c-to-b",
                ),
            ],
            write_claims: Vec::new(),
            evidence_issues: Vec::new(),
        };
        let merged = merge_hybrid_dag(&input);
        assert_eq!(merged.proposed_hard_edges.len(), 1);
        assert_eq!(merged.proposed_hard_edges[0].edge, input.model_edges[0]);
        assert!(merged
            .conflicts
            .iter()
            .any(|conflict| conflict.kind == HybridConflictKind::ReverseModelEdge));
        assert_eq!(
            merged
                .conflicts
                .iter()
                .filter(|conflict| conflict.kind == HybridConflictKind::CycleProducingAddition)
                .count(),
            2
        );
        assert!(!merged.automatic_adoption_allowed);
        let mut reordered = input;
        reordered.mechanical_edges.reverse();
        assert_eq!(merged, merge_hybrid_dag(&reordered));
    }

    #[test]
    fn hybrid_write_collision_is_undirected() {
        let merged = merge_hybrid_dag(&HybridMergeInput {
            task_ids: vec!["left".into(), "right".into()],
            model_edges: Vec::new(),
            mechanical_edges: Vec::new(),
            write_claims: vec![
                HybridWriteClaim {
                    task_id: "right".into(),
                    symbol_id: "same".into(),
                },
                HybridWriteClaim {
                    task_id: "left".into(),
                    symbol_id: "same".into(),
                },
            ],
            evidence_issues: Vec::new(),
        });
        assert!(merged.proposed_hard_edges.is_empty());
        let collision = merged
            .conflicts
            .iter()
            .find(|conflict| conflict.kind == HybridConflictKind::WriteWriteCollision)
            .unwrap();
        assert_eq!(collision.task_ids, vec!["left", "right"]);
        assert!(collision.edge.is_none());
    }

    #[test]
    fn hybrid_all_incomplete_states_fail_safe() {
        let states = [
            HybridEvidenceState::Partial,
            HybridEvidenceState::Unsupported,
            HybridEvidenceState::Timeout,
            HybridEvidenceState::Truncated,
            HybridEvidenceState::Ambiguous,
            HybridEvidenceState::Stale,
            HybridEvidenceState::Absent,
        ];
        for state in states {
            let merged = merge_hybrid_dag(&HybridMergeInput {
                task_ids: vec!["provider".into(), "consumer".into()],
                model_edges: Vec::new(),
                mechanical_edges: vec![hybrid_evidence(
                    "consumer",
                    "provider",
                    HybridEvidenceKind::ContractDirected,
                    state,
                    "index://degraded",
                )],
                write_claims: Vec::new(),
                evidence_issues: Vec::new(),
            });
            assert!(merged.proposed_hard_edges.is_empty(), "state={state:?}");
            assert_eq!(merged.degradations[0].state, state);
            assert!(!merged.automatic_adoption_allowed);
        }
    }

    #[test]
    fn hybrid_model_cycle_is_preserved_but_blocked() {
        let merged = merge_hybrid_dag(&HybridMergeInput {
            task_ids: vec!["a".into(), "b".into()],
            model_edges: vec![
                HybridTaskEdge {
                    from: "a".into(),
                    to: "b".into(),
                },
                HybridTaskEdge {
                    from: "b".into(),
                    to: "a".into(),
                },
            ],
            mechanical_edges: Vec::new(),
            write_claims: Vec::new(),
            evidence_issues: Vec::new(),
        });
        assert_eq!(merged.proposed_hard_edges.len(), 2);
        assert!(!merged.acyclic);
        assert!(!merged.automatic_adoption_allowed);
        // The structured decision must attribute the block to the cycle gate.
        assert_eq!(merged.adoption.verdict, HybridAdoptionVerdict::Block);
        assert!(merged
            .adoption
            .reasons
            .iter()
            .any(|r| r.code == HybridAdoptionReasonCode::CyclicHardDag));
    }

    #[test]
    fn adoption_decision_from_gates_is_exhaustive_and_ordered() {
        // All-green -> Adopt, no reasons.
        let clean = HybridAdoptionDecision::from_gates(true, 0, 0);
        assert_eq!(clean.verdict, HybridAdoptionVerdict::Adopt);
        assert!(clean.is_adopt());
        assert!(clean.reasons.is_empty());

        // Each single failing gate yields exactly its own reason with count.
        let cyc = HybridAdoptionDecision::from_gates(false, 0, 0);
        assert_eq!(cyc.verdict, HybridAdoptionVerdict::Block);
        assert_eq!(
            cyc.reasons,
            vec![HybridAdoptionReason {
                code: HybridAdoptionReasonCode::CyclicHardDag,
                count: 0,
            }]
        );
        let conf = HybridAdoptionDecision::from_gates(true, 3, 0);
        assert_eq!(
            conf.reasons,
            vec![HybridAdoptionReason {
                code: HybridAdoptionReasonCode::UnresolvedConflicts,
                count: 3,
            }]
        );
        let deg = HybridAdoptionDecision::from_gates(true, 0, 5);
        assert_eq!(
            deg.reasons,
            vec![HybridAdoptionReason {
                code: HybridAdoptionReasonCode::DegradedEvidence,
                count: 5,
            }]
        );

        // All three failing -> three reasons in the stable (sorted) order,
        // preserving each cause's own count.
        let all = HybridAdoptionDecision::from_gates(false, 2, 4);
        assert_eq!(all.verdict, HybridAdoptionVerdict::Block);
        assert!(!all.is_adopt());
        assert_eq!(all.reasons.len(), 3);
        // Sort is deterministic: recomputing yields byte-identical output.
        assert_eq!(all, HybridAdoptionDecision::from_gates(false, 2, 4));
        let codes: Vec<_> = all.reasons.iter().map(|r| r.code).collect();
        assert!(codes.contains(&HybridAdoptionReasonCode::CyclicHardDag));
        assert!(codes.contains(&HybridAdoptionReasonCode::UnresolvedConflicts));
        assert!(codes.contains(&HybridAdoptionReasonCode::DegradedEvidence));
    }

    #[test]
    fn merge_keeps_legacy_bool_in_lockstep_with_decision() {
        // Clean two-task merge with a trusted import edge -> Adopt.
        let merged = merge_hybrid_dag(&HybridMergeInput {
            task_ids: vec!["a".into(), "b".into()],
            model_edges: vec![HybridTaskEdge {
                from: "a".into(),
                to: "b".into(),
            }],
            mechanical_edges: Vec::new(),
            write_claims: Vec::new(),
            evidence_issues: Vec::new(),
        });
        assert!(merged.acyclic);
        assert!(merged.automatic_adoption_allowed);
        // The boolean is exactly the verdict's Adopt-ness.
        assert_eq!(
            merged.automatic_adoption_allowed,
            merged.adoption.is_adopt()
        );
        assert_eq!(merged.adoption.verdict, HybridAdoptionVerdict::Adopt);
        assert!(merged.adoption.reasons.is_empty());

        // Degraded evidence -> Block, bool false, degraded-evidence reason.
        let degraded = merge_hybrid_dag(&HybridMergeInput {
            task_ids: vec!["a".into(), "b".into()],
            model_edges: Vec::new(),
            mechanical_edges: Vec::new(),
            write_claims: Vec::new(),
            evidence_issues: vec![HybridEvidenceIssue {
                task_ids: vec!["a".into()],
                state: HybridEvidenceState::Stale,
                evidence_ref: "index://stale".into(),
            }],
        });
        assert!(!degraded.automatic_adoption_allowed);
        assert_eq!(
            degraded.automatic_adoption_allowed,
            degraded.adoption.is_adopt()
        );
        assert!(degraded
            .adoption
            .reasons
            .iter()
            .any(|r| r.code == HybridAdoptionReasonCode::DegradedEvidence));
    }
}

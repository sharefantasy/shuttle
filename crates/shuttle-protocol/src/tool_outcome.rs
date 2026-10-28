//! `ToolOutcome` — the universal "green or one-step-fixable" failure envelope
//! (efficiency bet #2).
//!
//! Every shuttle tool that an upstream planner/model drives should be able to answer
//! *one* question from its result without parsing free-text: **is this done, or
//! is there a single concrete next action that would make it done?** Today each
//! tool speaks its own dialect — refactor emits `confidence` + `warnings`, nav
//! emits a `Confidence`, the recipe gate rejects with a message, conflict emits
//! a receipt. A router (P2) that wants to decide *keep the cheap model vs.
//! escalate to the expensive one* has to special-case every shape.
//!
//! This module is that single shape. It lives in `shuttle-protocol` (the base crate,
//! depended on by everyone) and is deliberately self-contained: it references no
//! type from `shuttle-refactor`/`shuttle-nav`/`shuttle-conflict` (that would invert the
//! dependency graph). Instead each tool crate *lowers* its native result into a
//! [`ToolOutcome`] + a [`TypedDetail`] variant. The typed detail keeps the
//! tool-specific payload machine-readable without forcing the router to know it.
//!
//! # The contract (three states, never more)
//! * [`OutcomeState::Green`]   — the tool did its job; `detail` carries the result.
//! * [`OutcomeState::Blocked`] — the tool *could* proceed, but needs one concrete
//!   thing first. `whats_missing` + `next_action` (+ optional `force_hint`) say
//!   exactly what. This is the "one-step-fixable" state — a router can often
//!   resolve it mechanically (retry with `--force`, fetch a dropped type, pick an
//!   ambiguous candidate) *without* a model round-trip.
//! * [`OutcomeState::Error`]   — a genuine failure the tool cannot frame as a
//!   next action (unreadable file, parse crash, invalid input). Only this state
//!   should map to an MCP `isError: true` (see the MCP shim, P1.3).
//!
//! # Never guess (§7)
//! `Blocked` is *not* a soft error we paper over. When a tool blocks because a
//! decision is genuinely ambiguous (multiple candidate defs) it says so via
//! `reason_code = "ambiguous"` and lists the candidates in `whats_missing`; it
//! does **not** silently bind to the first hit and return `Green`.

use serde::{Deserialize, Serialize};

/// The three — and only three — states any tool result collapses to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeState {
    /// Done. `detail` carries the successful result.
    Green,
    /// Recoverable: one concrete next action would unblock it.
    Blocked,
    /// Genuine failure the tool cannot frame as a next action.
    Error,
}

impl OutcomeState {
    /// Only `Error` is a hard failure; `Blocked` is actionable, not failed.
    pub fn is_error(self) -> bool {
        matches!(self, OutcomeState::Error)
    }
    /// Machine string used in receipts / routing tables.
    pub fn as_str(self) -> &'static str {
        match self {
            OutcomeState::Green => "green",
            OutcomeState::Blocked => "blocked",
            OutcomeState::Error => "error",
        }
    }
}

/// Confidence mirror. `shuttle-nav` owns the canonical `Confidence`, but `shuttle-protocol`
/// is *below* `shuttle-nav` in the dependency graph, so we re-declare the same three
/// grades here for the envelope. Tool crates map `shuttle_nav::Confidence` → this
/// 1:1. Kept `Option` in details so tools without a confidence notion omit it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutcomeConfidence {
    High,
    Medium,
    Low,
}

/// Stable `reason_code` constants. Free-form codes are allowed (the field is a
/// `String`), but routing (P2.1) keys off these well-known ones, so tools should
/// prefer them. Grouped by the state they usually accompany.
pub mod reason {
    // --- Green ---
    /// Completed with full confidence.
    pub const OK: &str = "ok";
    /// Completed, but a best-effort/low-confidence result the caller may re-check.
    pub const OK_LOW_CONFIDENCE: &str = "ok-low-confidence";

    // --- Blocked (one-step-fixable) ---
    /// A legality gate refused; `force_hint` says how to override deliberately.
    pub const NEEDS_FORCE: &str = "needs-force";
    /// Resolution hit multiple candidates; `whats_missing` lists them (§7).
    pub const AMBIGUOUS: &str = "ambiguous";
    /// The result was truncated for budget; dropped items are the missing piece.
    pub const CONTEXT_TRUNCATED: &str = "context-truncated";
    /// A required name/type/def could not be found in scope; needs a wider scan
    /// or an upstream definition first.
    pub const MISSING_DEFINITION: &str = "missing-definition";
    /// A split/goal adjudication produced edges the machine cannot decide
    /// (CONFLICT/UNKNOWN); a human/model must review only those edges (§7 —
    /// never guess a direction the symbol graph cannot prove).
    pub const NEEDS_REVIEW: &str = "needs-review";
    /// The symbol graph the tool would use is stale relative to the requested
    /// commit (the worktree/index `base_commit_oid` does not match `--head`).
    /// Per the staleness watermark (C11), the tool refuses to reason off a
    /// stale graph — re-index at the requested commit is the one next action.
    pub const STALE_INDEX: &str = "stale-index";
    /// `shuttle verify --run`: a compile unit in the re-verify set failed to build;
    /// the change does not compile. `whats_missing` carries the failed units +
    /// stderr digests. Never reported as green (§7 — a broken build is not a
    /// passing test).
    pub const BUILD_BLOCKED: &str = "build-blocked";
    /// `shuttle verify --run`: the units built but a test in the re-verify set is
    /// red. `whats_missing` carries the failed tests + first failing assertion.
    pub const TEST_BLOCKED: &str = "test-blocked";
    /// `shuttle verify --run`: at least one unit could not be *run* at all — its
    /// build/test command is not in the cell binding allowlist, needs a laned
    /// side effect (net/clock/global state), or the test environment is
    /// missing. Per §7, "did not run" is never laundered into "passed".
    pub const NEEDS_RUN: &str = "needs-run";

    // --- Error (unframeable) ---
    /// Input the tool cannot act on (bad path, empty selection, bad args).
    pub const INVALID_INPUT: &str = "invalid-input";
    /// A file could not be read/written.
    pub const IO: &str = "io";
    /// A source file could not be parsed.
    pub const PARSE: &str = "parse";
    /// An internal invariant broke (bug); distinct from user-facing errors.
    pub const INTERNAL: &str = "internal";
}

/// Tool-specific payload, kept machine-readable so a router never parses prose.
/// Each variant is self-contained (primitive fields only) — tool crates build
/// these from their native results. `serde` tags on `kind` so the JSON is
/// self-describing on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TypedDetail {
    /// A refactor plan result (from `shuttle-refactor`). Carries the emitted edit
    /// plan inline so a `Green` outcome is directly appliable.
    Refactor(RefactorDetail),
    /// A rename result (cross-file symbol rename): how many sites, low-confidence
    /// sites the caller should eyeball.
    Rename(RenameDetail),
    /// A conflict / 3-way merge result (from `shuttle-conflict`).
    Conflict(ConflictDetail),
    /// A context-pack / type-closure slice (from `shuttle-refactor::type_closure`):
    /// what was included, what got dropped for budget, what stayed unresolved.
    Context(ContextDetail),
    /// A navigation result (defs/refs/callers/hover, from `shuttle-nav`).
    Nav(NavDetail),
    /// A split-correction adjudication (from `shuttle-nav::split`): the corrected
    /// task DAG with a per-edge three-state verdict (AGREE/ADD/DROP/CONFLICT/
    /// UNKNOWN) grounded in the symbol graph.
    Split(SplitDetail),
    /// Advisory symbol-derived task edges and touch hints. Unlike Split, this
    /// payload never adjudicates or rewrites the planner's declared DAG.
    SplitDerivation(SplitDerivationDetail),
    /// A land-time incremental re-verify set (from `shuttle-nav::reverify`): the
    /// minimal set of compile units to re-verify for a `base..head` diff =
    /// changed units ∪ their reverse-dependency closure, with a staleness
    /// watermark so a stale graph is never silently trusted (T2, §6.3-4).
    Reverify(ReverifyDetail),
    /// A land-time *execution* verification result (from `shuttle-nav::verify`, G5):
    /// the T2 re-verify set was actually built + tested inside the `cell-run`
    /// permission cage, and each unit's build/test/skip verdict folded into
    /// runtime evidence. This is the axis-2 "execution truth" payload — a Green
    /// here means "proven compiled + proven green", not "statically inferred".
    Verify(VerifyDetail),
    /// A recipe expansion / gate result (from `shuttle-refactor::recipe`).
    Recipe(RecipeDetail),
    /// A reverse-reconcile result (from `shuttle goal --reconcile`, G8-P3): a landed
    /// diff attributed back to the intent DAG — per-task satisfied/violated/
    /// no-trace plus the diff paths that fit no task (`unattributed`, never
    /// hard-attributed, §7).
    Reconcile(ReconcileDetail),
    /// No structured payload (a bare gate result, etc.).
    None,
}

/// Refactor detail — the edit plan plus its trust grade.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefactorDetail {
    /// The refactor kind (kebab string, e.g. "extract-method").
    pub op: String,
    /// The lowered edit plan, ready to apply when state == Green.
    pub plan: crate::EditPlan,
    pub confidence: OutcomeConfidence,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Rename detail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameDetail {
    pub from: String,
    pub to: String,
    /// Files touched.
    pub files: usize,
    /// Total edit sites.
    pub sites: usize,
    /// Sites the tool is unsure about (caller should review).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub low_confidence_sites: Vec<String>,
}

/// Conflict / merge detail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictDetail {
    pub path: String,
    /// Total hunks considered.
    pub hunks: usize,
    /// Hunks auto-resolved.
    pub auto_resolved: usize,
    /// Hunks still needing a human/model decision.
    pub unresolved: usize,
}

/// One node kept in a context slice (mirror of `type_closure::ClosureNode`,
/// primitive-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextNode {
    pub symbol: String,
    pub path: String,
    pub line: usize,
    pub confidence: OutcomeConfidence,
    pub est_tokens: usize,
}

/// One item that could not fit / could not resolve.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissingItem {
    pub symbol: String,
    /// "budget" (dropped) | "no-def" | "unreadable" | "ambiguous".
    pub reason: String,
}

/// Context-pack / type-closure detail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextDetail {
    pub root: String,
    pub root_path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub included: Vec<ContextNode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<MissingItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved: Vec<MissingItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ambiguous: Vec<String>,
    pub used_tokens: usize,
    pub budget_tokens: usize,
}

/// One navigation hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavHitLite {
    pub symbol: String,
    pub path: String,
    pub line: usize,
    pub confidence: OutcomeConfidence,
}

/// Navigation detail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavDetail {
    /// "defs" | "references" | "callers" | "impls" | "hover".
    pub query: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hits: Vec<NavHitLite>,
}

/// The three-state verdict the splitter assigns to every candidate dependency
/// edge (). Ordering is by
/// increasing need for attention so a router can sort on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeVerdict {
    /// The author's intent edge is corroborated by the symbol graph. Keep.
    Agree,
    /// The symbol graph proves a dependency the author omitted. Add it
    /// (`source = symbolgraph`), never silently — it is reported as an add.
    Add,
    /// The author declared an edge the symbol graph proves redundant. Suggest
    /// dropping it, with the evidence; the author's intent is not overridden.
    Drop,
    /// Intent and symbol graph disagree on a proven edge. Escalate to review.
    Conflict,
    /// The machine cannot prove or disprove the edge (no import corroboration,
    /// ambiguous callee, or a symbol outside the scanned universe). Escalate to
    /// review — never guessed either way (§7).
    Unknown,
}

impl EdgeVerdict {
    /// Machine string for receipts / routing.
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeVerdict::Agree => "agree",
            EdgeVerdict::Add => "add",
            EdgeVerdict::Drop => "drop",
            EdgeVerdict::Conflict => "conflict",
            EdgeVerdict::Unknown => "unknown",
        }
    }
    /// Whether this verdict must surface on the human/model review gate. Only
    /// `Conflict`/`Unknown` do — `Agree`/`Add`/`Drop` are mechanically decided.
    pub fn needs_review(self) -> bool {
        matches!(self, EdgeVerdict::Conflict | EdgeVerdict::Unknown)
    }
}

/// Provenance of a corrected edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeSource {
    /// The edge came from the author's/model's intent DAG only.
    Intent,
    /// The edge was proven by the symbol graph only (a machine add).
    Symbolgraph,
    /// The edge is present in both intent and symbol graph (an agree).
    Both,
}

impl EdgeSource {
    /// Machine string for receipts / routing.
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeSource::Intent => "intent",
            EdgeSource::Symbolgraph => "symbolgraph",
            EdgeSource::Both => "both",
        }
    }
}

/// One adjudicated dependency edge in the corrected DAG.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitEdge {
    /// Prerequisite task id (the `from` a `to` depends on).
    pub from: String,
    /// Dependent task id.
    pub to: String,
    pub verdict: EdgeVerdict,
    pub confidence: OutcomeConfidence,
    pub source: EdgeSource,
    /// Machine-readable evidence for the verdict (e.g. the symbol pair and the
    /// import direction that proved it), safe to show but not parsed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub evidence: String,
}

/// One task node carried through the correction, echoing the intent id/label and
/// the files the splitter attributed to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitNode {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub intent: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub touched_files: Vec<String>,
}

/// Split-correction detail — the corrected task DAG plus the three-state
/// verdict per edge and the review surface (only CONFLICT/UNKNOWN edges).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitDetail {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub goal: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<SplitNode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<SplitEdge>,
    /// Count of edges the machine added that the author omitted (`Add`).
    pub added: usize,
    /// Count of author edges the machine flags redundant (`Drop`).
    pub dropped: usize,
    /// Count of edges escalated to review (`Conflict` + `Unknown`).
    pub review_surface: usize,
    /// Edge indices (into `edges`) requiring review — only CONFLICT/UNKNOWN.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub review_required: Vec<usize>,
}

/// One symbol reference carried by split-derivation evidence.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DerivationSymbolRef {
    pub name: String,
    pub path: String,
    pub kind: String,
}

/// One task edge lowered from a symbol dependency. Direction is prerequisite
/// task to dependent task; the underlying symbol edge runs consumer to
/// provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedTaskEdge {
    pub from: String,
    pub to: String,
    pub from_sym: DerivationSymbolRef,
    pub to_sym: DerivationSymbolRef,
    pub confidence: OutcomeConfidence,
    pub kind: String,
}

/// Per-task symbol evidence and mechanically expanded touch hint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedTask {
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_symbols: Vec<DerivationSymbolRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_symbols: Vec<DerivationSymbolRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub touch_hint: Vec<String>,
}

/// Machine payload for advisory split derivation. Any incomplete proof sets
/// fallback_required so the output cannot silently authorize scheduling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitDerivationDetail {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub goal: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<DerivedTask>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hard_edges: Vec<DerivedTaskEdge>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weak_hints: Vec<DerivedTaskEdge>,
    pub index_status: String,
    pub max_depth: usize,
    pub max_fanout: usize,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub truncation_reasons: Vec<String>,
    pub acyclic: bool,
    pub fallback_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_reason: Option<String>,
}

/// One compile unit in a land-time re-verify set. `reason` distinguishes a unit
/// that was *directly changed* by the diff from one pulled in only by the
/// reverse-dependency closure — consumers can prioritize the changed units and
/// treat the closure as the safety margin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReverifyUnit {
    /// The compile unit's path (file granularity; Go package is T4).
    pub path: String,
    /// "changed" (in the base..head diff) | "reverse-dep" (reached via closure).
    pub reason: String,
    /// Weakest confidence along the closure path that reached this unit; a
    /// directly changed unit is always `High` (the diff is authoritative).
    pub confidence: OutcomeConfidence,
    /// Reverse-closure distance from the nearest changed unit (0 = changed).
    pub distance: usize,
}

/// Land-time incremental re-verify detail (T2). The minimal set a lander must
/// re-verify before merging a `base..head` diff, computed at compile-unit
/// granularity as *changed units ∪ reverse-dependency closure* (C12). Carries
/// the staleness watermark (C11): `index_base_oid` is the commit the live
/// symbol graph actually reflects; when it disagrees with `head_oid` the set is
/// `degraded` and the envelope blocks with `reason::STALE_INDEX` — shuttle never
/// reasons off a stale graph, and #11 keeps shuttle out of the land itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReverifyDetail {
    /// The merge base / pre-change commit the diff is taken from.
    pub base_oid: String,
    /// The post-change commit the diff lands.
    pub head_oid: String,
    /// The commit the live symbol graph actually reflects (worktree HEAD). When
    /// this differs from `head_oid`, `degraded` is set (staleness watermark).
    pub index_base_oid: String,
    /// Files changed between base..head, straight from `git diff` — always
    /// commit-authoritative, independent of the (possibly stale) graph.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_files: Vec<String>,
    /// The minimal re-verify set = changed ∪ reverse-closure, sorted
    /// deterministically (changed-first, then by path).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reverify: Vec<ReverifyUnit>,
    /// Granularity label of the units ("file"; Go package alignment is T4).
    pub granularity: String,
    /// True when the graph was stale vs `head_oid` — the reverse-closure is a
    /// best-effort ctags degrade and must not be trusted as complete (C11).
    pub degraded: bool,
    /// Cardinality of `reverify` (the headline metric fed back to T5 telemetry).
    pub reverify_size: usize,
}

// -- G5 execution-truth: build/test the T2 re-verify set inside the cage. --

/// Machine-readable `kind` of a [`FailedUnit`] — which stage failed. Kept as
/// stable strings so a router keys off them without parsing prose.
pub mod verify_kind {
    /// The unit failed to *compile* (`build-blocked`).
    pub const BUILD: &str = "build";
    /// The unit compiled but its test was *red* (`test-blocked`).
    pub const TEST: &str = "test";
}

/// Machine-readable `reason` of a [`SkippedUnit`] — why a unit was not run. A
/// `not-allowed` / `env-missing` skip is a *blocking* skip (it forces
/// `needs-run`); a `no-test` skip is benign (a non-test unit legitimately has
/// nothing to run) and never blocks on its own.
pub mod skip_reason {
    /// The unit is a non-test compile unit — nothing to run, benign.
    pub const NO_TEST: &str = "no-test";
    /// The unit's build/test command is not in the cell binding allowlist —
    /// shuttle will not spawn an unauthorized command (§7, #11). Blocking.
    pub const NOT_ALLOWED: &str = "not-allowed";
    /// The test environment is missing / would need a laned side effect
    /// (net/clock/global state) outside the cage. Blocking.
    pub const ENV_MISSING: &str = "env-missing";
}

impl SkippedUnit {
    /// A skip that must force `needs-run` (we *could not* verify, so §7 forbids
    /// calling it green). `no-test` is the only benign, non-blocking skip.
    pub fn is_blocking(&self) -> bool {
        self.reason != skip_reason::NO_TEST
    }
}

/// One re-verify unit whose build or test failed, with the evidence that proves
/// it (a stderr digest / first failing assertion) — safe to show, not parsed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedUnit {
    /// The compile unit path (mirrors [`ReverifyUnit::path`]).
    pub unit: String,
    /// Which stage failed: [`verify_kind::BUILD`] or [`verify_kind::TEST`].
    pub kind: String,
    /// Machine-safe evidence (stderr digest / first failing assertion).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub evidence: String,
}

/// One re-verify unit that was *not* run, and why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedUnit {
    /// The compile unit path.
    pub unit: String,
    /// Why it was skipped: one of [`skip_reason`].
    pub reason: String,
}

/// Land-time execution-verification detail (G5, axis-2). The T2 re-verify set
/// was partitioned into test / non-test units and each test unit was built +
/// run inside the `cell-run` cage; this records the runtime verdict. A Green
/// [`ToolOutcome`] built from this is *proven* (compiled + tested), not merely
/// statically inferred — but only when nothing was left unrun (`needs-run`),
/// nothing failed, and no unknown edge forced `partial`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyDetail {
    /// The merge base / pre-change commit (echoes [`ReverifyDetail::base_oid`]).
    pub base_oid: String,
    /// The post-change commit the diff lands.
    pub head_oid: String,
    /// The paper re-verify cardinality T2 asserted (before running anything).
    pub reverify_size: usize,
    /// How many units were actually built + run.
    pub ran: usize,
    /// How many ran units passed (build + test).
    pub passed: usize,
    /// Units whose build or test failed, with evidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<FailedUnit>,
    /// Units that were not run, and why (`no-test` is benign; the rest block).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedUnit>,
    /// True when the re-verify set contained an UNKNOWN symbol-graph edge (e.g.
    /// a dynamic import, A5): the ran parts may be green but the whole cannot be
    /// claimed fully verified — surfaced as Green_low, never full green (§7).
    pub partial: bool,
    /// Transpose of [`ReverifyDetail::degraded`] — the index was stale vs
    /// `head_oid`, so the re-verify set itself may be incomplete.
    pub degraded: bool,
}

/// Tool name for the execution-verify command (`shuttle verify --run`).
pub const TOOL_VERIFY: &str = "verify";

impl VerifyDetail {
    /// Failed units of a given [`verify_kind`].
    fn failures_of<'a>(&'a self, kind: &str) -> Vec<&'a FailedUnit> {
        self.failed.iter().filter(|f| f.kind == kind).collect()
    }
    /// Skips that force `needs-run` (everything but `no-test`).
    fn blocking_skips(&self) -> Vec<&SkippedUnit> {
        self.skipped.iter().filter(|s| s.is_blocking()).collect()
    }
}

/// Downgrade a [`VerifyDetail`] to a [`ToolOutcome`] per the G5 §3.3/§4 rules.
///
/// Precedence is by *authority of the signal*: an observed build/test failure is
/// ground truth and dominates (report it regardless of index freshness); then
/// "could not run" (`needs-run`) and "stale graph" (`stale-index`) are the two
/// "we could not fully prove it" blockers; only when everything ran and passed
/// do we reach green, and an UNKNOWN edge caps that at Green_low (`partial`).
/// Per §7, a unit that did not run is *never* laundered into a pass.
pub fn verify_outcome(detail: VerifyDetail) -> ToolOutcome {
    let build_fails = detail.failures_of(verify_kind::BUILD);
    let test_fails = detail.failures_of(verify_kind::TEST);
    let blocking_skips = detail.blocking_skips();

    if !build_fails.is_empty() {
        let missing: Vec<String> = build_fails
            .iter()
            .map(|f| format!("{} (build): {}", f.unit, f.evidence))
            .collect();
        return ToolOutcome::blocked(
            TOOL_VERIFY,
            reason::BUILD_BLOCKED,
            "fix the failing build, then re-run `shuttle verify --run`",
            TypedDetail::Verify(detail),
        )
        .with_missing(missing)
        .with_summary("re-verify set does not compile");
    }
    if !test_fails.is_empty() {
        let missing: Vec<String> = test_fails
            .iter()
            .map(|f| format!("{} (test): {}", f.unit, f.evidence))
            .collect();
        return ToolOutcome::blocked(
            TOOL_VERIFY,
            reason::TEST_BLOCKED,
            "fix the failing test(s), then re-run `shuttle verify --run`",
            TypedDetail::Verify(detail),
        )
        .with_missing(missing)
        .with_summary("affected test(s) are red");
    }
    if !blocking_skips.is_empty() {
        let missing: Vec<String> = blocking_skips
            .iter()
            .map(|s| format!("{} ({})", s.unit, s.reason))
            .collect();
        return ToolOutcome::blocked(
            TOOL_VERIFY,
            reason::NEEDS_RUN,
            "authorize the build/test command in the cell binding, then re-run",
            TypedDetail::Verify(detail),
        )
        .with_missing(missing)
        .with_summary("some units could not be run — not claiming green (§7)");
    }
    if detail.degraded {
        return ToolOutcome::blocked(
            TOOL_VERIFY,
            reason::STALE_INDEX,
            "re-index at head then re-run `shuttle verify` (the graph is stale)",
            TypedDetail::Verify(detail),
        )
        .with_missing(vec![
            "symbol graph stale vs head — re-verify set may be incomplete".to_string(),
        ])
        .with_summary("stale index — re-verify set not trustworthy");
    }
    if detail.partial {
        return ToolOutcome::green_low(TOOL_VERIFY, TypedDetail::Verify(detail)).with_summary(
            "ran parts green, but an UNKNOWN symbol-graph edge leaves the whole unproven (partial)",
        );
    }
    ToolOutcome::green(TOOL_VERIFY, TypedDetail::Verify(detail))
        .with_summary("affected build + tests proven green")
}

/// Recipe expansion / gate detail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeDetail {
    pub recipe_id: String,
    /// Number of steps the recipe expands to.
    pub steps: usize,
    /// Whether the recipe passed its legality gate.
    pub gate_passed: bool,
}

// -- G8-P3 reverse-reconcile: attribute a landed diff back to intent tasks. --

/// The three-state verdict of matching one intent task against a landed diff
/// (design §4.3). Stable `snake_case` strings so a router / reviewer keys off
/// them without parsing prose. There is deliberately **no** verdict for
/// "probably this task" — an unattributable trace is [`ReconcileDetail::unattributed`],
/// never hard-attributed to the nearest task (§7 never-guess red line).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReconcileVerdict {
    /// The diff has a provable trace (a changed file) inside this task's
    /// declared `touched_files_guess` set — the task's footprint was honoured.
    Satisfied,
    /// The diff conflicts with this task's declared intent: it changed a file
    /// the task named only as a prerequisite dependency (claimed not to touch),
    /// carried as machine evidence in [`ReconciledTask::evidence`].
    Violated,
    /// This intent task has **no** trace in the diff at all — the plan claimed
    /// it, the diff does not corroborate it. Reported, never fabricated into a
    /// weak "maybe satisfied".
    NoTrace,
}

impl ReconcileVerdict {
    /// Stable wire string (mirrors the serde rename).
    pub fn as_str(&self) -> &'static str {
        match self {
            ReconcileVerdict::Satisfied => "satisfied",
            ReconcileVerdict::Violated => "violated",
            ReconcileVerdict::NoTrace => "no-trace",
        }
    }
}

/// One intent task adjudicated against the landed diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciledTask {
    /// The intent task id (mirrors `IntentTask::id`).
    pub id: String,
    /// The three-state verdict for this task.
    pub verdict: ReconcileVerdict,
    /// The diff paths that landed inside this task's declared footprint
    /// (the mechanical trace behind a `satisfied` verdict). Empty for `no-trace`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_files: Vec<String>,
    /// Machine-safe evidence for a `violated` verdict (which claimed-but-untouched
    /// file the diff hit, etc.) — safe to show, not parsed. Empty otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub evidence: String,
}

/// Reverse-reconcile detail (G8-P3, design §4.2/§4.3). The inverse of
/// `GoalTelemetry`: given a landed diff (its changed-file set) and the intent
/// DAG that claimed to produce it, re-derive which tasks the diff **satisfied**,
/// **violated**, or left **no-trace**, plus the diff paths that fit **no** task
/// at all (`unattributed`). Its cardinal rule is §7: an `unattributed` path is
/// *never* hard-attributed to the nearest-looking task — it is surfaced as-is
/// so a reviewer sees exactly what the mechanical evidence could and could not
/// place. A non-empty `unattributed` blocks the outcome (`needs-review`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcileDetail {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub goal: String,
    /// Every changed file in the diff, as parsed — commit-authoritative,
    /// independent of any graph.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_files: Vec<String>,
    /// Per-task verdicts, in the intent DAG's task order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<ReconciledTask>,
    /// Diff paths that fit **no** intent task's declared footprint. NEVER
    /// hard-attributed (§7): reported verbatim for a human to place.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unattributed: Vec<String>,
    /// Count of `satisfied` tasks.
    pub satisfied: usize,
    /// Count of `violated` tasks.
    pub violated: usize,
    /// Count of `no-trace` tasks (claimed, uncorroborated).
    pub no_trace: usize,
}

/// Tool name for the reverse-reconcile command (`shuttle goal --reconcile`).
pub const TOOL_RECONCILE: &str = "reconcile";

/// Lower a [`ReconcileDetail`] into the universal envelope (design §4.3). The
/// state machine, worst-first: any `violated` task => `Blocked(needs-review)`
/// (broken intent, must be looked at); else any `unattributed` path =>
/// `Blocked(needs-review)` (a landed change no task claims — never guessed onto
/// one); else a clean attribution => `Green`. A `no-trace` task alone is *not*
/// blocking here (the diff simply did not do a claimed task — informational for
/// review), so it degrades to `Green_low` rather than green.
pub fn reconcile_outcome(detail: ReconcileDetail) -> ToolOutcome {
    if detail.violated > 0 {
        let missing: Vec<String> = detail
            .tasks
            .iter()
            .filter(|t| t.verdict == ReconcileVerdict::Violated)
            .map(|t| {
                if t.evidence.is_empty() {
                    format!("{}: violated", t.id)
                } else {
                    format!("{}: {}", t.id, t.evidence)
                }
            })
            .collect();
        let summary = format!(
            "reconcile: {} satisfied / {} violated / {} no-trace / {} unattributed path(s)",
            detail.satisfied,
            detail.violated,
            detail.no_trace,
            detail.unattributed.len()
        );
        return ToolOutcome::blocked(
            TOOL_RECONCILE,
            reason::NEEDS_REVIEW,
            "the diff conflicts with a claimed intent — review the violated task(s)",
            TypedDetail::Reconcile(detail),
        )
        .with_missing(missing)
        .with_summary(summary);
    }
    if !detail.unattributed.is_empty() {
        let missing = detail.unattributed.clone();
        let summary = format!(
            "reconcile: {} satisfied / 0 violated / {} no-trace / {} unattributed path(s)",
            detail.satisfied,
            detail.no_trace,
            detail.unattributed.len()
        );
        return ToolOutcome::blocked(
            TOOL_RECONCILE,
            reason::NEEDS_REVIEW,
            "a landed change fits no intent task — attribute it manually (never guessed)",
            TypedDetail::Reconcile(detail),
        )
        .with_missing(missing)
        .with_summary(summary);
    }
    let summary = format!(
        "reconcile: {} satisfied / 0 violated / {} no-trace / 0 unattributed",
        detail.satisfied, detail.no_trace
    );
    if detail.no_trace > 0 {
        return ToolOutcome::green_low(TOOL_RECONCILE, TypedDetail::Reconcile(detail))
            .with_summary(summary);
    }
    ToolOutcome::green(TOOL_RECONCILE, TypedDetail::Reconcile(detail)).with_summary(summary)
}

/// The universal envelope. Serialize this from every tool; a router reads
/// `state` + `reason_code` (+ `confidence`-ish signals inside `detail`) to pick
/// a model tier, and a human/model reads `whats_missing` + `next_action` to fix
/// a `Blocked` result in one step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutcome {
    /// Envelope schema version (independent of `EditPlan`'s version).
    pub version: u32,
    /// Which tool produced this (e.g. "refactor", "context-pack", "nav").
    pub tool: String,
    pub state: OutcomeState,
    /// Stable code (prefer [`reason`] constants) for routing.
    pub reason_code: String,
    /// Human-readable one-liner (safe to show; not parsed by the router).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// For `Blocked`: the concrete things missing (candidate list, dropped
    /// symbols, the def that isn't in scope). Empty for `Green`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub whats_missing: Vec<String>,
    /// For `Blocked`: the single next action that would unblock (imperative,
    /// e.g. "re-run with --scan-scope" or "choose one of the candidates").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
    /// For `Blocked` + `needs-force`: the exact flag/token to override the gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_hint: Option<String>,
    /// Tool-specific machine-readable payload.
    pub detail: TypedDetail,
}

/// Current envelope schema version.
pub const OUTCOME_VERSION: u32 = 1;

impl ToolOutcome {
    /// A `Green` outcome carrying a typed detail.
    pub fn green(tool: impl Into<String>, detail: TypedDetail) -> Self {
        ToolOutcome {
            version: OUTCOME_VERSION,
            tool: tool.into(),
            state: OutcomeState::Green,
            reason_code: reason::OK.to_string(),
            summary: None,
            whats_missing: Vec::new(),
            next_action: None,
            force_hint: None,
            detail,
        }
    }

    /// A `Green`-but-low-confidence outcome (best-effort result).
    pub fn green_low(tool: impl Into<String>, detail: TypedDetail) -> Self {
        let mut o = Self::green(tool, detail);
        o.reason_code = reason::OK_LOW_CONFIDENCE.to_string();
        o
    }

    /// A `Blocked` outcome: recoverable, with the one next action spelled out.
    pub fn blocked(
        tool: impl Into<String>,
        reason_code: impl Into<String>,
        next_action: impl Into<String>,
        detail: TypedDetail,
    ) -> Self {
        ToolOutcome {
            version: OUTCOME_VERSION,
            tool: tool.into(),
            state: OutcomeState::Blocked,
            reason_code: reason_code.into(),
            summary: None,
            whats_missing: Vec::new(),
            next_action: Some(next_action.into()),
            force_hint: None,
            detail,
        }
    }

    /// A genuine `Error` outcome (maps to MCP isError).
    pub fn error(
        tool: impl Into<String>,
        reason_code: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        ToolOutcome {
            version: OUTCOME_VERSION,
            tool: tool.into(),
            state: OutcomeState::Error,
            reason_code: reason_code.into(),
            summary: Some(summary.into()),
            whats_missing: Vec::new(),
            next_action: None,
            force_hint: None,
            detail: TypedDetail::None,
        }
    }

    /// Builder: attach `whats_missing` items.
    pub fn with_missing(mut self, missing: Vec<String>) -> Self {
        self.whats_missing = missing;
        self
    }

    /// Builder: attach a `force_hint`.
    pub fn with_force_hint(mut self, hint: impl Into<String>) -> Self {
        self.force_hint = Some(hint.into());
        self
    }

    /// Builder: attach a human summary.
    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn green_serializes_without_blocked_fields() {
        let o = ToolOutcome::green("nav", TypedDetail::None);
        let j = serde_json::to_value(&o).unwrap();
        assert_eq!(j["state"], "green");
        assert_eq!(j["reason_code"], "ok");
        // Blocked-only fields are omitted when empty/none.
        assert!(j.get("whats_missing").is_none());
        assert!(j.get("next_action").is_none());
        assert!(j.get("force_hint").is_none());
    }

    #[test]
    fn blocked_needs_force_carries_actionable_fields() {
        let o = ToolOutcome::blocked(
            "refactor",
            reason::NEEDS_FORCE,
            "re-run with --force to override the low-confidence gate",
            TypedDetail::None,
        )
        .with_force_hint("--force")
        .with_missing(vec!["confidence below High".to_string()]);
        assert_eq!(o.state, OutcomeState::Blocked);
        assert!(!o.state.is_error());
        let j = serde_json::to_value(&o).unwrap();
        assert_eq!(j["state"], "blocked");
        assert_eq!(j["reason_code"], "needs-force");
        assert_eq!(j["force_hint"], "--force");
        assert!(j["next_action"].as_str().unwrap().contains("--force"));
    }

    #[test]
    fn error_is_the_only_hard_failure() {
        let o = ToolOutcome::error("context-pack", reason::PARSE, "cannot index root file");
        assert!(o.state.is_error());
        assert_eq!(o.state.as_str(), "error");
        // round-trips
        let s = serde_json::to_string(&o).unwrap();
        let back: ToolOutcome = serde_json::from_str(&s).unwrap();
        assert_eq!(back.state, OutcomeState::Error);
        assert_eq!(back.reason_code, "parse");
    }

    #[test]
    fn context_detail_roundtrips_with_dropped_and_unresolved() {
        let d = TypedDetail::Context(ContextDetail {
            root: "sync".into(),
            root_path: "main.py".into(),
            included: vec![ContextNode {
                symbol: "Report".into(),
                path: "types.py".into(),
                line: 4,
                confidence: OutcomeConfidence::High,
                est_tokens: 15,
            }],
            dropped: vec![MissingItem {
                symbol: "Big".into(),
                reason: "budget".into(),
            }],
            unresolved: vec![MissingItem {
                symbol: "Conn".into(),
                reason: "no-def".into(),
            }],
            ambiguous: vec![],
            used_tokens: 15,
            budget_tokens: 256,
        });
        let o = ToolOutcome::blocked(
            "context-pack",
            reason::CONTEXT_TRUNCATED,
            "re-fetch dropped symbols precisely, or raise --budget",
            d,
        )
        .with_missing(vec!["Big (budget)".into(), "Conn (no-def)".into()]);
        let s = serde_json::to_string(&o).unwrap();
        let back: ToolOutcome = serde_json::from_str(&s).unwrap();
        match back.detail {
            TypedDetail::Context(c) => {
                assert_eq!(c.included.len(), 1);
                assert_eq!(c.dropped[0].reason, "budget");
                assert_eq!(c.unresolved[0].symbol, "Conn");
            }
            _ => panic!("wrong detail variant"),
        }
    }

    #[test]
    fn split_detail_roundtrips_with_verdicts() {
        let d = TypedDetail::Split(SplitDetail {
            goal: "make X E2E".into(),
            nodes: vec![
                SplitNode {
                    id: "t1".into(),
                    intent: "define core".into(),
                    touched_files: vec!["a.py".into()],
                },
                SplitNode {
                    id: "t2".into(),
                    intent: "wrap core".into(),
                    touched_files: vec!["b.py".into()],
                },
            ],
            edges: vec![
                SplitEdge {
                    from: "t1".into(),
                    to: "t2".into(),
                    verdict: EdgeVerdict::Agree,
                    confidence: OutcomeConfidence::Medium,
                    source: EdgeSource::Both,
                    evidence: "core: b.py imports a.py".into(),
                },
                SplitEdge {
                    from: "t2".into(),
                    to: "t1".into(),
                    verdict: EdgeVerdict::Unknown,
                    confidence: OutcomeConfidence::Low,
                    source: EdgeSource::Intent,
                    evidence: String::new(),
                },
            ],
            added: 0,
            dropped: 0,
            review_surface: 1,
            review_required: vec![1],
        });
        let o = ToolOutcome::blocked(
            "split",
            reason::NEEDS_REVIEW,
            "review the 1 conflict/unknown edge",
            d,
        )
        .with_missing(vec!["edge t2->t1: unknown".into()]);
        assert_eq!(o.state, OutcomeState::Blocked);
        assert!(!o.state.is_error());
        let s = serde_json::to_string(&o).unwrap();
        let back: ToolOutcome = serde_json::from_str(&s).unwrap();
        assert_eq!(back.reason_code, "needs-review");
        match back.detail {
            TypedDetail::Split(sp) => {
                assert_eq!(sp.edges.len(), 2);
                assert_eq!(sp.edges[0].verdict, EdgeVerdict::Agree);
                assert_eq!(sp.edges[0].verdict.as_str(), "agree");
                assert!(!sp.edges[0].verdict.needs_review());
                assert!(sp.edges[1].verdict.needs_review());
                assert_eq!(sp.review_required, vec![1]);
            }
            _ => panic!("wrong detail variant"),
        }
    }

    #[test]
    fn reverify_detail_roundtrips_and_blocks_when_stale() {
        let d = TypedDetail::Reverify(ReverifyDetail {
            base_oid: "aaaa".into(),
            head_oid: "bbbb".into(),
            index_base_oid: "cccc".into(),
            changed_files: vec!["core.py".into()],
            reverify: vec![
                ReverifyUnit {
                    path: "core.py".into(),
                    reason: "changed".into(),
                    confidence: OutcomeConfidence::High,
                    distance: 0,
                },
                ReverifyUnit {
                    path: "wrapper.py".into(),
                    reason: "reverse-dep".into(),
                    confidence: OutcomeConfidence::Medium,
                    distance: 1,
                },
            ],
            granularity: "file".into(),
            degraded: true,
            reverify_size: 2,
        });
        let o = ToolOutcome::blocked(
            "impact",
            reason::STALE_INDEX,
            "re-index at bbbb (graph reflects cccc) then re-run",
            d,
        )
        .with_missing(vec!["graph base cccc != requested head bbbb".into()]);
        assert_eq!(o.state, OutcomeState::Blocked);
        assert!(!o.state.is_error());
        let s = serde_json::to_string(&o).unwrap();
        let back: ToolOutcome = serde_json::from_str(&s).unwrap();
        assert_eq!(back.reason_code, "stale-index");
        match back.detail {
            TypedDetail::Reverify(r) => {
                assert!(r.degraded);
                assert_eq!(r.reverify_size, 2);
                assert_eq!(r.reverify[0].reason, "changed");
                assert_eq!(r.reverify[0].distance, 0);
                assert_eq!(r.reverify[1].reason, "reverse-dep");
                assert_eq!(r.granularity, "file");
            }
            _ => panic!("wrong detail variant"),
        }
    }

    // ---- G5 verify_outcome downgrade matrix (§3.3/§4) ----

    fn vd() -> VerifyDetail {
        // A clean, fully-green base detail; each test mutates one axis.
        VerifyDetail {
            base_oid: "aaaa".into(),
            head_oid: "bbbb".into(),
            reverify_size: 2,
            ran: 2,
            passed: 2,
            failed: vec![],
            skipped: vec![],
            partial: false,
            degraded: false,
        }
    }

    #[test]
    fn verify_all_green_is_green() {
        let o = verify_outcome(vd());
        assert_eq!(o.state, OutcomeState::Green);
        assert_eq!(o.reason_code, reason::OK);
        assert_eq!(o.tool, "verify");
        // round-trips with the Verify detail intact.
        let s = serde_json::to_string(&o).unwrap();
        let back: ToolOutcome = serde_json::from_str(&s).unwrap();
        match back.detail {
            TypedDetail::Verify(v) => {
                assert_eq!(v.ran, 2);
                assert_eq!(v.passed, 2);
                assert!(!v.partial);
            }
            _ => panic!("wrong detail variant"),
        }
    }

    #[test]
    fn verify_build_fail_blocks_build_blocked() {
        let mut d = vd();
        d.passed = 1;
        d.failed = vec![FailedUnit {
            unit: "core.py".into(),
            kind: verify_kind::BUILD.into(),
            evidence: "E0433: cannot find crate".into(),
        }];
        let o = verify_outcome(d);
        assert_eq!(o.state, OutcomeState::Blocked);
        assert_eq!(o.reason_code, reason::BUILD_BLOCKED);
        assert!(o.whats_missing[0].contains("core.py (build)"));
    }

    #[test]
    fn verify_test_fail_blocks_test_blocked() {
        let mut d = vd();
        d.passed = 1;
        d.failed = vec![FailedUnit {
            unit: "wrapper_test.py".into(),
            kind: verify_kind::TEST.into(),
            evidence: "assert 1 == 2".into(),
        }];
        let o = verify_outcome(d);
        assert_eq!(o.state, OutcomeState::Blocked);
        assert_eq!(o.reason_code, reason::TEST_BLOCKED);
        assert!(o.whats_missing[0].contains("(test)"));
    }

    #[test]
    fn verify_build_fail_dominates_test_fail() {
        // Ground-truth precedence: a build failure is reported before a test one.
        let mut d = vd();
        d.failed = vec![
            FailedUnit {
                unit: "b_test.py".into(),
                kind: verify_kind::TEST.into(),
                evidence: "red".into(),
            },
            FailedUnit {
                unit: "core.py".into(),
                kind: verify_kind::BUILD.into(),
                evidence: "no compile".into(),
            },
        ];
        let o = verify_outcome(d);
        assert_eq!(o.reason_code, reason::BUILD_BLOCKED);
    }

    #[test]
    fn verify_blocking_skip_blocks_needs_run() {
        let mut d = vd();
        d.ran = 1;
        d.passed = 1;
        d.skipped = vec![
            SkippedUnit {
                unit: "helper.py".into(),
                reason: skip_reason::NO_TEST.into(),
            },
            SkippedUnit {
                unit: "net_test.py".into(),
                reason: skip_reason::NOT_ALLOWED.into(),
            },
        ];
        let o = verify_outcome(d);
        assert_eq!(o.state, OutcomeState::Blocked);
        assert_eq!(o.reason_code, reason::NEEDS_RUN);
        // Only the blocking skip surfaces in whats_missing; no-test is benign.
        assert_eq!(o.whats_missing.len(), 1);
        assert!(o.whats_missing[0].contains("net_test.py (not-allowed)"));
    }

    #[test]
    fn verify_no_test_skip_alone_is_still_green() {
        // A non-test unit legitimately has nothing to run — must NOT force needs-run.
        let mut d = vd();
        d.ran = 1;
        d.passed = 1;
        d.skipped = vec![SkippedUnit {
            unit: "helper.py".into(),
            reason: skip_reason::NO_TEST.into(),
        }];
        let o = verify_outcome(d);
        assert_eq!(o.state, OutcomeState::Green);
        assert_eq!(o.reason_code, reason::OK);
    }

    #[test]
    fn verify_env_missing_skip_blocks_needs_run() {
        let mut d = vd();
        d.ran = 1;
        d.passed = 1;
        d.skipped = vec![SkippedUnit {
            unit: "db_test.py".into(),
            reason: skip_reason::ENV_MISSING.into(),
        }];
        let o = verify_outcome(d);
        assert_eq!(o.reason_code, reason::NEEDS_RUN);
    }

    #[test]
    fn verify_partial_unknown_edge_is_green_low() {
        let mut d = vd();
        d.partial = true;
        let o = verify_outcome(d);
        assert_eq!(o.state, OutcomeState::Green);
        // Green, but explicitly low-confidence — never laundered to full OK.
        assert_eq!(o.reason_code, reason::OK_LOW_CONFIDENCE);
        assert!(o.summary.unwrap().contains("partial"));
    }

    #[test]
    fn verify_degraded_blocks_stale_index() {
        let mut d = vd();
        d.degraded = true;
        let o = verify_outcome(d);
        assert_eq!(o.state, OutcomeState::Blocked);
        assert_eq!(o.reason_code, reason::STALE_INDEX);
    }

    #[test]
    fn verify_failure_dominates_degraded() {
        // An observed test failure is ground truth even if the index is stale.
        let mut d = vd();
        d.degraded = true;
        d.failed = vec![FailedUnit {
            unit: "t.py".into(),
            kind: verify_kind::TEST.into(),
            evidence: "boom".into(),
        }];
        let o = verify_outcome(d);
        assert_eq!(o.reason_code, reason::TEST_BLOCKED);
    }

    #[test]
    fn verify_needs_run_dominates_degraded_and_partial() {
        // "could not run" outranks stale/partial: we simply have not proven it.
        let mut d = vd();
        d.degraded = true;
        d.partial = true;
        d.skipped = vec![SkippedUnit {
            unit: "x_test.py".into(),
            reason: skip_reason::NOT_ALLOWED.into(),
        }];
        let o = verify_outcome(d);
        assert_eq!(o.reason_code, reason::NEEDS_RUN);
    }

    #[test]
    fn verify_empty_diff_is_honest_green() {
        // Nothing to verify (empty diff) folds to an honest green, not a block.
        let d = VerifyDetail {
            base_oid: "aaaa".into(),
            head_oid: "aaaa".into(),
            reverify_size: 0,
            ran: 0,
            passed: 0,
            failed: vec![],
            skipped: vec![],
            partial: false,
            degraded: false,
        };
        let o = verify_outcome(d);
        assert_eq!(o.state, OutcomeState::Green);
        assert_eq!(o.reason_code, reason::OK);
    }
}

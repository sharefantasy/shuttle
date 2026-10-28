//! `shuttle` â the fast executor CLI.
//!
//! This is the binary a host agent (pi / aider) invokes to apply a plan.
//! It reads an [`EditPlan`] (JSON) on stdin or `--plan`, routes each op to
//! the right engine, and emits [`ApplyOutcome`]s as JSON on stdout.
//!
//! Contract with the upstream planner: the planner never rewrites files; it
//! only emits search/replace blocks, inserts, file creations, and codemod
//! invocations. This binary is the "fast small model's hands".

use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use shuttle_protocol::{ApplyOutcome, EditOp, EditPlan, InsertPosition, ScopedPathResolver};
use std::io::Read;
use std::path::PathBuf;

mod cell_registry;
mod cell_run;
mod cell_verify;
mod config;
mod conflict;
mod contextpack;
mod goal;
mod goal_cli;
mod provider_receipt;
mod refactor;
mod resource;
mod run;
mod sha256;
mod ui;
mod upstream;
mod usage_receipt;
mod verify;

#[derive(Parser)]
#[command(name = "shuttle", version, about = "Shuttle fast executor")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum AdviceVersion {
    Auto,
    V1,
    V2,
}

#[derive(Subcommand)]
enum Cmd {
    /// Apply an EditPlan (JSON via --plan file or stdin).
    Apply {
        #[arg(long)]
        plan: Option<String>,
        /// Repo root for codemod / locate scope.
        #[arg(long, default_value = ".")]
        root: String,
        /// Preview only; do not write to disk.
        #[arg(long)]
        dry_run: bool,
        /// â¢ Gate mode: probe every op in-process (dry-run) and commit the
        /// writes ONLY if all resolve â one process, plan parsed once,
        /// all-or-nothing. Aligns with P0-2 `Rejected` fail-closed.
        #[arg(long)]
        gate: bool,
        /// P0-2: confine every write to `root` via ScopedPathResolver.
        /// Under a cell profile this is forced on and cannot be disabled.
        #[arg(long = "enforce-scope")]
        enforce_scope: bool,
        /// P0-2: allowlist (relative to root) consumed when --enforce-scope is
        /// set. FAIL-CLOSED: empty + enforce rejects every write. Pass `**` to
        /// allow the whole subtree, or e.g. `src/**,tests/**` to narrow it.
        #[arg(long = "allowed-path", value_delimiter = ',')]
        allowed_paths: Vec<String>,
    },
    /// Locate anchors (debug helper).
    Locate {
        #[arg(long)]
        pattern: String,
        #[arg(long)]
        lang: Option<String>,
        #[arg(long, default_value = ".")]
        root: String,
    },
    /// Emit a token-budgeted repo-map for the upstream planner (Â§7.5).
    Repomap {
        /// Token budget for the emitted map.
        #[arg(long, default_value_t = 1024)]
        budget: usize,
        /// Files the current EditPlan touches (personalized-PageRank weighting).
        #[arg(long = "focus")]
        focus: Vec<String>,
        #[arg(long, default_value = ".")]
        root: String,
        /// â  Â§7.5 incremental cache (ON by default): reuse `.shuttle/symbols.cache`,
        /// re-running ctags only on files whose mtime+size changed (plus any
        /// --focus hints). A cold/absent cache does one full scan and seeds it.
        /// Pass --no-cache to force a cold whole-repo scan.
        #[arg(long = "no-cache", action = ArgAction::SetTrue)]
        no_cache: bool,
        /// Cache file path (relative to cwd) used when --cache is set.
        #[arg(long = "cache-path", default_value = ".shuttle/symbols.cache")]
        cache_path: String,
        /// â¤ Narrow the ctags scan to the focus neighborhood (git-tracked
        /// files with a reference edge to --focus) instead of the whole repo.
        /// Takes precedence over --cache; no-op without --focus.
        #[arg(long = "scan-scope")]
        scan_scope: bool,
    },
    /// Rename a symbol across the repo via the symbol table (Â§7 tier 1 / Â§9.5).
    /// Rename a symbol across the repo. Dry-run by default: prints a
    /// `planned N / known M sites` reconciliation header plus the line-precise
    /// diff with per-site confidence. The default plan covers definition
    /// sites; reference sites fold in only with `--include-refs` (each keeping
    /// its High/Medium/Low tier). `--write` on a plan with uncovered sites is
    /// refused — a partial rename is never written silently.
    ///
    /// Flag independence: `--force` does NOT imply `--write` (it only lets
    /// Low-confidence sites be written, and without `--write` nothing is ever
    /// written). Typical combinations:
    ///   preview                      `shuttle rename --from A --to B`
    ///   write defs only (gap-free)   ... --write
    ///   write defs + references      ... --write --include-refs
    ///   ...also Low (text-tier) refs ... --write --include-refs --force`
    Rename {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long, default_value = ".")]
        root: String,
        /// Apply the rename to disk (default is dry-run preview).
        #[arg(long)]
        write: bool,
        /// Fold reference sites (from the references pipeline) into the plan,
        /// each keeping its confidence tier. Without it, a plan with reference
        /// gaps is blocked from writing.
        #[arg(long)]
        include_refs: bool,
        /// Allow writing even when some sites are Low-confidence (§9.5 gate).
        #[arg(long)]
        force: bool,
        /// Emit JSON (RenamePlan) instead of the human diff.
        #[arg(long)]
        json: bool,
    },
    /// references â every use of a symbol, tagged High/Medium/Low (Â§9.4).
    References {
        #[arg(long)]
        symbol: String,
        #[arg(long, default_value = ".")]
        root: String,
        /// Restrict to a symbol kind (function|method|class|struct|enum|variable|constant|module).
        #[arg(long)]
        kind: Option<String>,
        /// â¤ narrow the candidate set to the focus neighborhood of the defs.
        #[arg(long = "scan-scope")]
        scan_scope: bool,
        #[arg(long)]
        json: bool,
    },
    /// symbols â documentSymbol (--file) or workspace/symbol (--query) (Â§9.6).
    Symbols {
        /// Outline a single file.
        #[arg(long)]
        file: Option<String>,
        /// Fuzzy name query across the whole repo.
        #[arg(long)]
        query: Option<String>,
        #[arg(long, default_value = ".")]
        root: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// defs â every definition/declaration site of a symbol (Â§9.6).
    Defs {
        #[arg(long)]
        symbol: String,
        #[arg(long, default_value = ".")]
        root: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// impls â sites implementing/extending a type, High/Medium (Â§9.7).
    Impls {
        #[arg(long = "type")]
        type_name: String,
        #[arg(long, default_value = ".")]
        root: String,
        #[arg(long)]
        json: bool,
    },
    /// callers â incoming call sites of a function, capped at Medium (Â§9.7).
    Callers {
        #[arg(long)]
        symbol: String,
        #[arg(long, default_value = ".")]
        root: String,
        #[arg(long)]
        json: bool,
    },
    /// dataflow — G6 · S3: provable forward dataflow of a seed variable inside
    /// its enclosing function. Follows ONLY provable identifier-copy assignment
    /// chains; records call/return sinks (the value leaves the function) and
    /// unprovable stores (subscript / attribute / tuple targets) as frontiers —
    /// never propagating past what it can prove (§7 «绝不瞎猜»: 推不动即 None,
    /// 绝不做启发式污点扩散). Weakly-typed langs (Python/JS) tag edges Low,
    /// strongly-typed (Go/Rust/TS) Medium — never High.
    Dataflow {
        /// The seed local binding (parameter or variable) to trace.
        #[arg(long)]
        seed: String,
        /// File the seed lives in (analysis is intra-procedural within it).
        #[arg(long)]
        path: String,
        /// Max copy-edge hops from the seed.
        #[arg(long = "max-depth", default_value_t = 32)]
        max_depth: usize,
        #[arg(long)]
        json: bool,
    },
    /// impact â reverse-dependency closure (blast radius) of the symbols
    /// defined in the changed files. Walks the native symbol graph backwards:
    /// name the files a patch touches and shuttle returns every symbol that could
    /// break, tagged with confidence (weakest hop wins). `--tests` narrows the
    /// result to test files â the minimal set worth re-running. Daemonless
    /// realization of the SCIP blast radius .
    Impact {
        /// Changed files whose defined symbols seed the closure (repeatable).
        /// Mutually exclusive with the `--base/--head` land-time mode below;
        /// one of the two input modes is required.
        #[arg(long = "changed", num_args = 1.., conflicts_with_all = ["base", "head"])]
        changed: Vec<String>,
        /// T2 land-time mode: the merge base / pre-change commit. When set with
        /// `--head`, shuttle computes the minimal re-verify set for that diff
        /// (changed units ∪ reverse-dependency closure) at file granularity,
        /// with a staleness watermark. Requires `--head`.
        #[arg(long, requires = "head")]
        base: Option<String>,
        /// T2 land-time mode: the post-change commit the diff lands. Requires
        /// `--base`. shuttle only computes the set â it never lands (#11).
        #[arg(long, requires = "base")]
        head: Option<String>,
        #[arg(long, default_value = ".")]
        root: String,
        /// Max reverse-edge hops to traverse (0 = seeds only).
        #[arg(long = "max-depth", default_value_t = 64)]
        max_depth: usize,
        /// Narrow the result to test files only.
        #[arg(long)]
        tests: bool,
        /// T3: follow only trusted import-directed edges, yielding the acyclic
        /// zero-model DAG closure (excludes weak, direction-unproven name-match
        /// edges). Default off preserves the broad name-match recall.
        #[arg(long)]
        directed: bool,
        /// T5: in --base/--head land mode, also emit the reverify_size as a
        /// single JSON line to stderr (feeds the goal-decomposition ledger).
        /// stdout stays the ToolOutcome envelope. No-op outside land mode.
        #[arg(long)]
        telemetry: bool,
        /// G6 · S1: cross-language contract-graph impact. Treats `--changed`
        /// as changed IDL files (proto/thrift/...) and answers which impl files
        /// each declared message/RPC provably (via codegen anchoring on ≥2
        /// languages) touches — minting only `ContractDirected` edges. Nodes
        /// that codegen on one side, drifted with no codegen, or belong to a
        /// known-but-unparsed IDL dialect are surfaced as §7 degrades, never a
        /// green edge. Requires `--changed`; ignores `--base/--head`.
        /// G6 · S2: type-aware impact. On top of the trusted **directed**
        /// closure, keep only the callers that provably *type-depend* on a
        /// changed symbol (their cross-file type closure references it) — a
        /// result strictly NARROWER than `--directed`. Weakly-typed languages
        /// (Python/JS), unresolved type refs, or an unreadable caller degrade
        /// to keeping the directed edge at Medium (§7 «绝不瞎猜» — never
        /// pretend to understand types). Composes with `--changed`/`--tests`;
        /// ignores `--base/--head` and is mutually exclusive with `--contract`.
        #[arg(long)]
        typed: bool,
        #[arg(long)]
        contract: bool,
        /// G7 · U1c: also surface the file-level **co-change hint** — files that
        /// have historically been edited together with `--changed` in git
        /// history — as a review aid. This is a naturally weak signal: it is
        /// NEVER folded into the mechanical re-verify / blast-radius set, only
        /// printed as a separate hint block (or JSON key). A shallow / squashed
        /// history reports `insufficient-history` rather than extrapolate (§7).
        /// Composes with `--changed`; ignored in `--base/--head` land mode.
        #[arg(long)]
        cochange: bool,
        /// G7 · U1c: bound how far back the co-change scan reads git history
        /// (`0` = all history) so a huge repo stays inside the daemonless
        /// latency budget. No-op unless `--cochange` is set.
        #[arg(long = "cochange-max-commits", default_value_t = 500)]
        cochange_max_commits: usize,
        #[arg(long)]
        json: bool,
    },
    /// split --correct â adjudicate a planner's intent DAG against the directional
    /// symbol graph (T1). Reads an `intent/v1` plan.json, classifies each guessed
    /// dependency edge as AGREE/ADD/DROP (auto) or CONFLICT/UNKNOWN (review), and
    /// emits a `ToolOutcome` (Green when all decided, Blocked/needs-review else).
    /// shuttle never re-plans and never overrides the author's intent in place (Â§6.3).
    Split {
        /// Adjudicate the plan (currently the only mode). Present for forward
        /// compatibility with future split subcommands.
        #[arg(long, conflicts_with = "derive_edges")]
        correct: bool,
        /// Derive advisory DAG edges and touch hints from a caller-owned fresh
        /// symbol index. Never reads or rewrites the planner's declared edges.
        #[arg(long = "derive-edges", conflicts_with = "correct")]
        derive_edges: bool,
        /// Path to the intent DAG (`intent/v1`) JSON to correct.
        #[arg(long)]
        plan: String,
        /// Repo root whose symbol graph is the mechanical ground truth.
        #[arg(long, default_value = ".")]
        root: String,
        /// Caller-owned pure symbol-index artifact (required for derivation).
        #[arg(long = "symbol-index", requires = "derive_edges")]
        symbol_index: Option<String>,
        /// Revision the supplied symbol index must exactly represent.
        #[arg(long = "index-head", requires = "derive_edges")]
        index_head: Option<String>,
        /// Maximum reverse-dependency hops per task write set.
        #[arg(long = "max-depth", default_value_t = 64)]
        max_depth: usize,
        /// Maximum trusted reverse callers expanded per symbol.
        #[arg(long = "max-fanout", default_value_t = 256)]
        max_fanout: usize,
        /// T5: also emit a `SplitFeedback` JSON line to stderr - counts how
        /// much the mechanical graph corrected the planner (agree/add/drop/
        /// review surface). stdout stays the ToolOutcome envelope; the ledger
        /// is derived downstream to feed back into goal decomposition.
        #[arg(long)]
        telemetry: bool,
        /// G9·D2: also write a replayable provenance ledger to this path — one
        /// `(edge, verdict, evidence_digest, snapshot_oid)` record per adjudicated
        /// edge. It stores only the minimal replayable seed (a `sha256:` digest
        /// shuttle mints over the evidence), NEVER the full evidence text, so a caller
        /// can re-derive the evidence at `snapshot_oid`, re-hash, and confirm the
        /// verdict bit-for-bit. stdout stays the unchanged ToolOutcome envelope.
        #[arg(long)]
        ledger: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// goal — the §4 GoalGraph outer loop around `shuttle run` (G3). Decompose a
    /// GOAL into a candidate intent DAG (via `--goal` goal-mode pi or a
    /// pre-produced `--plan intent.json`), adjudicate every guessed dependency
    /// against the directional symbol graph (T1+T3 → three-state GoalGraph),
    /// run the surviving tasks in topological order, and — when `--base/--head`
    /// are given — compute the land-time minimal re-verify set (T2). shuttle never
    /// lands (#11) and never guesses: an undecided edge (CONFLICT/UNKNOWN) or a
    /// cycle escalates to a human-review surface instead of executing. Exit is
    /// always 0; the machine result (phase/escalated/review) is the contract.
    Goal {
        /// Durable Goal lifecycle command. When absent, the legacy GoalGraph
        /// outer-loop flags below remain available for compatibility.
        #[command(subcommand)]
        command: Option<Box<goal_cli::GoalCmd>>,
        /// The GOAL to decompose live via a goal-mode pi child. Mutually
        /// exclusive with `--plan`; exactly one of the two is required.
        #[arg(long, conflicts_with = "plan")]
        goal: Option<String>,
        /// A pre-produced `intent/v1` DAG JSON, read verbatim instead of
        /// spawning pi (the hermetic / external-/human-produced path). Mutually
        /// exclusive with `--goal`.
        #[arg(long, conflicts_with = "goal")]
        plan: Option<String>,
        /// Repo root whose symbol graph is the mechanical ground truth.
        #[arg(long, default_value = ".")]
        root: String,
        /// Model pattern passed to pi (goal-mode decompose + per-task execute).
        #[arg(long)]
        model: Option<String>,
        /// Thinking level: off/minimal/low/medium/high/xhigh/max.
        #[arg(long)]
        thinking: Option<String>,
        /// pi provider to select (default from shuttle.toml or env when configured).
        #[arg(long)]
        provider: Option<String>,
        /// T2 land-time mode: the merge base / pre-change commit. With `--head`,
        /// shuttle computes the minimal re-verify set after all tasks land. Requires
        /// `--head`.
        #[arg(long, requires = "head")]
        base: Option<String>,
        /// T2 land-time mode: the post-change commit. Requires `--base`.
        #[arg(long, requires = "base")]
        head: Option<String>,
        /// Max reverse-edge hops for the land-time re-verify closure.
        #[arg(long = "max-depth", default_value_t = 64)]
        max_depth: usize,
        /// Swap the per-task executor for a no-op: exercise the whole outer
        /// loop (adjudicate → topo → land) with zero model calls (hermetic).
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// G8/P1: also compute a concurrency schedule — partition the topo order
        /// into batches of tasks proven file- AND symbol-disjoint (so a launcher
        /// could run each batch concurrently). Any pair shuttle cannot prove
        /// independent stays serial (§7). Execution order is unchanged; the
        /// schedule is an additional hint on the result.
        #[arg(long)]
        parallel: bool,
        /// Write the digest-bound parallel advice projection of the final
        /// corrected schedule to this path. Defaults to the compatible
        /// Auto-selects `shuttle_goal_parallel_advice/v2` when a canonical symbol
        /// footprint is present, otherwise emits compatible v1 advice. The file is advice only;
        /// it grants no execution or land authority.
        #[arg(
            long,
            requires = "parallel",
            requires = "plan",
            conflicts_with = "reconcile"
        )]
        advice_out: Option<String>,
        /// Parallel-advice wire version. auto selects v2 with a canonical
        /// `shuttle_symbol_footprint/v1` sidecar and otherwise falls back to v1.
        #[arg(
            long = "advice-version",
            value_enum,
            default_value_t = AdviceVersion::Auto,
            requires = "advice_out"
        )]
        advice_version: AdviceVersion,
        /// Canonical snapshot-bound `shuttle_symbol_footprint/v1` input used to
        /// derive Hybrid v2 evidence. The CLI derives all planning, base and
        /// index bindings from this sidecar and the frozen `--plan`; callers
        /// cannot inject duplicate binding metadata.
        #[arg(long = "symbol-footprint", requires = "advice_out")]
        symbol_footprint: Option<String>,
        /// G8/P2: a per-task observed-footprint map JSON
        /// (`{"<id>": ["<path>", ...]}`) handed in by the launcher/external/test. Any
        /// file outside a task's attributed set is *drift* → shuttle re-plans the
        /// remaining tasks (§3.2). Absent in `--dry-run` ⇒ no observation; absent
        /// in the live path ⇒ shuttle reads the git working-tree delta per task.
        #[arg(long)]
        observe: Option<String>,
        /// G8/P2: resume from a persisted `GoalCheckpoint` JSON of a prior
        /// (interrupted/escalated) session. Its completed tasks are skipped
        /// (never redone) and the re-plan tally continues (anti-thrash across
        /// resumes). shuttle owns no checkpoint store (#11): you persist the
        /// `checkpoint` field of a prior result and hand it back here.
        #[arg(long)]
        resume: Option<String>,
        /// T5: also emit the end-to-end `GoalTelemetry` as a single JSON line to
        /// stderr (feeds the next round of decomposition). stdout stays the
        /// machine result.
        #[arg(long)]
        telemetry: bool,
        /// G8/P3b reverse-reconcile mode: instead of driving the goal, read the
        /// intent DAG from `--plan` and a *landed* unified diff from `--diff`,
        /// then re-derive per intent task whether the diff satisfies /
        /// violates / leaves no trace of it, and list any diff path that fits
        /// no task (`unattributed` — never hard-attributed to the nearest
        /// task, §4.3). shuttle owns no land权 (#11): the diff is an INPUT. Requires
        /// `--plan` and `--diff`; ignores the executor entirely.
        #[arg(long, requires = "diff", requires = "plan")]
        reconcile: bool,
        /// G8/P3b: path to the landed unified diff to reconcile against the
        /// intent DAG. Only meaningful with `--reconcile`.
        #[arg(long)]
        diff: Option<String>,
        /// G9·D2: also write a replayable provenance ledger of the adjudicated
        /// GoalGraph to this path — one `(edge, verdict, evidence_digest,
        /// snapshot_oid)` record per split edge, anchored to the current HEAD.
        /// Stores only the `sha256:` digest seed shuttle mints over each edge's
        /// evidence, NEVER the full evidence text (§2 anti-bloat / §7 replay:
        /// re-derive at `snapshot_oid`, re-hash, confirm bit-for-bit). stdout
        /// stays the unchanged GoalResult envelope.
        #[arg(long)]
        ledger: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// verify — execution truth (G5, docs/shuttle_verify_runtime_truth_design.md).
    /// The land-time counterpart of `shuttle impact --base/--head`: it computes the
    /// same minimal re-verify set (T2) and, with `--run`, actually builds +
    /// tests each affected unit *inside a cell-run cage* — so "green" means the
    /// code was really run, never that we assumed it. Default (no `--run`) is a
    /// pure compute-only alias of `shuttle impact` (zero behavior change). `--run`
    /// spawns only allowlist-authorized toolchains; a command outside the
    /// allowlist is a blocking `needs-run`, a stale index degrades to
    /// `stale-index`, and an UNKNOWN frontier (`--partial`) caps an otherwise
    /// green run at `Green_low`. `--dry-run` drives the identical loop with a
    /// hermetic no-op runner (preview, zero toolchain). shuttle never lands (#11)
    /// and never launders "did not run" into "passed" (§7). Exit is always 0.
    Verify {
        /// The merge base / pre-change commit. Required (with `--head`).
        #[arg(long, requires = "head")]
        base: String,
        /// The post-change commit the diff lands. Required (with `--base`).
        #[arg(long, requires = "base")]
        head: String,
        /// Repo root whose symbol graph is the mechanical ground truth and, in
        /// `--run` mode, the (canonicalized) cage root the toolchains run in.
        #[arg(long, default_value = ".")]
        root: String,
        /// Actually build + test each affected unit inside the cage (execution
        /// truth). Default off = compute-only alias of `shuttle impact`. Mutually
        /// exclusive with `--dry-run`.
        #[arg(long, conflicts_with = "dry_run")]
        run: bool,
        /// Drive the full state machine with a hermetic zero-side-effect runner
        /// (every unit "builds" + "passes", no subprocess). A preview of the
        /// loop with no toolchain present. Mutually exclusive with `--run`.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Path to a `cell_binding/v1` JSON authorizing the `--run` cage
        /// (validated fail-closed). Absent → shuttle synthesizes a default local
        /// cage authorizing only the native build/test toolchains it can drive
        /// (cargo/go/pytest/npm). No-op outside `--run`/`--dry-run`.
        #[arg(long)]
        binding: Option<String>,
        /// Max reverse-edge hops for the land-time re-verify closure.
        #[arg(long = "max-depth", default_value_t = shuttle_nav::verify::DEFAULT_MAX_DEPTH)]
        max_depth: usize,
        /// Assert the changed symbols had an UNKNOWN outgoing symbol-graph edge
        /// (e.g. an A5 dynamic import) the trusted closure did not follow: even
        /// an all-green run is capped at `Green_low` (partial), never full
        /// green — we never claim to have covered a frontier we cannot prove.
        /// Until the select stage carries it natively (G6 semantic graph), the
        /// caller supplies it; default off means "no UNKNOWN frontier detected".
        #[arg(long)]
        partial: bool,
        /// T5: also emit the `VerifyTelemetry` as a single JSON line to stderr
        /// (a pure projection of what ran). stdout stays the ToolOutcome
        /// envelope. In compute-only mode this is the `reverify_size` line, as
        /// `shuttle impact --telemetry`.
        #[arg(long)]
        telemetry: bool,
        #[arg(long)]
        json: bool,
    },

    /// hover â signature + adjacent doc comment for a symbol, High (Â§9.7).
    Hover {
        #[arg(long)]
        symbol: String,
        #[arg(long, default_value = ".")]
        root: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// backends â report AST-grammar availability per language (Â§9.8 diagnostic).
    /// Reads sgconfig.yml customLanguages and classifies each supported language
    /// as Builtin / CustomRegistered / CustomUnregistered. Pure explainability:
    /// no capability behaviour changes, it only shows which langs can reach High.
    Backends {
        #[arg(long, default_value = ".")]
        root: String,
        #[arg(long)]
        json: bool,
    },
    /// G9·D3 â calibrate the High/Medium/Low confidence floors from
    /// execution-truth samples (§4). Reads a `CalibrationInput` JSON (one
    /// `TierSample` per tier + optional G7 co-change control) and emits the
    /// `CalibratedThresholds` verdict. A sample below the minimum gate freezes
    /// the default floor (never extrapolate); the provable `High` tier is
    /// pinned and never down-adjusted. Pure projection: no repo access.
    Calibrate {
        /// Path to a `CalibrationInput` JSON (high/medium/low TierSample, plus
        /// an optional `cochange_control` pooled into Low).
        #[arg(long)]
        input: String,
        #[arg(long)]
        json: bool,
    },
    /// G9·D1 â run shuttle as an LSP *server* over stdio (daemonless, no port).
    /// The editor spawns this process and talks JSON-RPC over its pipes; shuttle
    /// answers `textDocument/hover` and `shuttle/impact` with its reverse-closure
    /// impact + caller confidence, then exits on `exit`/EOF. It never binds a
    /// socket or listens on a port (Â§2.2); each request is the same one-shot
    /// pure call the CLI makes, so cold-start is 0.
    Lsp {
        /// Idle window (seconds) after which the loop treats the session as
        /// idle; a spawn-per-session editor simply re-spawns on the next need.
        #[arg(long = "idle-secs", default_value_t = 300)]
        idle_secs: u64,
    },
    /// typedef — §9.9 tier-3: resolve the *type* of the expression at a
    /// position via a REAL external language server (goto type-definition, or
    /// —with `--type-hierarchy`— the super/subtype hierarchy). This is the
    /// ONLY shuttle capability that links a language server, and it does so behind
    /// the OFF-by-default `real-lsp` feature: the stock binary answers with an
    /// honest "compiled LSP-free — rebuild with --features real-lsp". Every hit
    /// is tagged "via external LSP" and 用完即走 (the server is spawned,
    /// asked one question, and killed — never resident, never a port). Language
    /// is routed from the file extension; an unsupported language, a missing
    /// server binary, and an empty/anonymous result are each reported honestly,
    /// never fabricated (§7 「绝不瞎猜」).
    Typedef {
        /// File containing the expression to resolve (relative to --root, or
        /// absolute). Its extension selects the language server.
        #[arg(long)]
        path: String,
        /// 1-based line of the expression (matches editor gutters; lowered to
        /// LSP 0-based internally).
        #[arg(long)]
        line: usize,
        /// 0-based column of the expression (LSP-native).
        #[arg(long)]
        col: usize,
        /// The symbol name at that position (used only to tag the resulting
        /// NavHit; the server resolves by position, not by name).
        #[arg(long, default_value = "")]
        symbol: String,
        /// Ask for the type *hierarchy* (super/subtypes) instead of the single
        /// goto-type-definition.
        #[arg(long = "type-hierarchy")]
        type_hierarchy: bool,
        #[arg(long, default_value = ".")]
        root: String,
        #[arg(long)]
        json: bool,
    },
    /// Run a single named codemod rule (ast-grep) â zero model tokens (Â§ tier 2).
    Codemod {
        #[arg(long)]
        rule_id: String,
        #[arg(long, default_value = "rules")]
        rules_dir: String,
        #[arg(long, default_value = ".")]
        root: String,
        /// Preview matches only; do not write to disk.
        #[arg(long)]
        dry_run: bool,
    },
    /// List available codemod rule ids in the rules directory.
    Rules {
        #[arg(long, default_value = "rules")]
        rules_dir: String,
    },
    /// â¡ Assemble the upstream contract a leaf subtask needs â in ONE process.
    /// Resolves depends_on + touch_hint (from the split tasks dir or explicit
    /// flags), picks the handoff files, and emits a token-budgeted symbol
    /// skeleton via a single reused SymbolTable. Replaces the plugin's
    /// multi-spawn orchestration (kills W-B).
    ContextPack {
        /// Path to the split `tasks/` dir holding task-*.yaml.
        #[arg(long = "tasks-dir")]
        tasks_dir: Option<String>,
        /// Current subtask id (e.g. task-31); its depends_on + touch_hint drive the pack.
        #[arg(long = "task-id")]
        task_id: Option<String>,
        /// Explicit predecessor ids, overriding the yaml depends_on.
        #[arg(long = "depends-on", value_delimiter = ',')]
        depends_on: Vec<String>,
        /// Explicit files this task touches, overriding the yaml touch_hint.
        #[arg(long = "touch-hint", value_delimiter = ',')]
        touch_hint: Vec<String>,
        /// Git ref/range for the no-metadata fallback (`git diff --name-only <base>`).
        #[arg(long = "git-base")]
        git_base: Option<String>,
        /// Caller-owned pure symbol-index artifact. Omit to preserve the
        /// legacy ctags-only output shape exactly.
        #[arg(long = "symbol-index")]
        symbol_index: Option<String>,
        /// Revision the symbol index must exactly represent (defaults to the
        /// current repository HEAD).
        #[arg(long = "index-head", requires = "symbol_index")]
        index_head: Option<String>,
        /// Explicit current-task write evidence, encoded as NAME@REPO_PATH.
        /// Every seed must resolve uniquely in a fresh index; unresolved or
        /// ambiguous seeds are omitted rather than guessed.
        #[arg(long = "write-symbol", value_delimiter = ',')]
        write_symbols: Vec<String>,
        /// Token budget for the emitted symbol skeleton.
        #[arg(long, default_value_t = 1024)]
        budget: usize,
        #[arg(long, default_value = ".")]
        root: String,
        /// â¤ Narrow the symbol scan to the focus neighborhood (git-tracked
        /// files referencing the handoff/focus set) instead of the whole repo.
        #[arg(long = "scan-scope")]
        scan_scope: bool,
    },
    /// Supervise a pi planning loop (architecture reversal â see
    /// docs/shuttle_wraps_pi_architecture.md). shuttle is the host; pi is the replaceable
    /// LLM planner spawned as a child with edit/write disabled.
    Run {
        /// The requirement / prompt handed to the pi planner.
        prompt: String,
        /// Model pattern passed to pi (default from shuttle.toml or built-in).
        #[arg(long)]
        model: Option<String>,
        /// Thinking level: off/minimal/low/medium/high/xhigh/max.
        #[arg(long)]
        thinking: Option<String>,
        /// pi provider to select (default from shuttle.toml or env when configured).
        #[arg(long)]
        provider: Option<String>,
        /// RPC deep-drive (form C) instead of the default json supervisor (form B).
        #[arg(long)]
        deep: bool,
        /// Built-in pi tools to disable (shuttle takes these over).
        #[arg(long = "exclude-tools", value_delimiter = ',')]
        exclude_tools: Option<Vec<String>>,
        /// shuttle extension file injected into pi via -e.
        #[arg(long = "extension", short = 'e')]
        extension: Option<String>,
        /// Repo root pi runs in.
        #[arg(long, default_value = ".")]
        root: String,
        /// Don't persist a pi session (ephemeral eval runs).
        #[arg(long = "no-session")]
        no_session: bool,
        /// Raw pi args after `--` (escape hatch: never lose a pi capability).
        #[arg(last = true)]
        passthrough: Vec<String>,
    },
    /// Run shuttle as a caged cell agent under a shuttle-local `cell_binding/v1`
    /// envelope (fail-closed: writes are confined to the binding's
    /// allowed_paths, no scope escape hatch â P0-1). The binding may be a
    /// minimal hand-written file (direct agent launch) or produced by
    /// `CellBinding::from_external` when external orchestrates the run.
    CellRun {
        /// Path to the cell_binding/v1 JSON envelope. Reads stdin if omitted.
        #[arg(long = "binding")]
        binding: Option<String>,
        /// Task prompt handed to the caged pi child. Without it, cell-run only
        /// validates the binding + prints preflight (no spawn).
        #[arg(long = "prompt")]
        prompt: Option<String>,
        /// Model pattern passed to the caged pi child (default from shuttle.toml or
        /// built-in). The cage owns tool/scope policy, not model selection, so
        /// callers (e.g. the eval B arm) can still pin the provider model here.
        #[arg(long)]
        model: Option<String>,
        /// external-orchestrated ingest: path to a `external_launch_bundle/v1` JSON (or `-`
        /// / omitted-with-stdin). When set, shuttle lowers the carried external contracts
        /// (`external_launch_intent` + `external_context_manifest` + `external_resource_claim`)
        /// into the binding via `CellBinding::from_external` and emits a
        /// `external_provider_receipt/v1`. Mutually exclusive with `--binding`.
        #[arg(long = "launch-bundle", conflicts_with = "binding")]
        launch_bundle: Option<String>,
        /// Where to write the `external_provider_receipt/v1` (external path only):
        /// a file path, or `-` for stdout. Ignored without `--launch-bundle`.
        #[arg(long = "receipt-out")]
        receipt_out: Option<String>,
    },
    /// Read the durable state of one cell run without changing it.
    CellObserve {
        #[arg(long = "run-id")]
        run_id: String,
    },
    /// Return the latest compatible five-field receipt for one cell run.
    CellCollect {
        #[arg(long = "run-id")]
        run_id: String,
    },
    /// Return the immutable shuttle_usage_receipt/v1 for one terminal cell run.
    CellUsage {
        #[arg(long = "run-id")]
        run_id: String,
    },
    /// Cancel one cell process group, TERM then KILL after the grace window.
    CellCancel {
        #[arg(long = "run-id")]
        run_id: String,
        #[arg(long = "grace-ms", default_value_t = 10_000)]
        grace_ms: u64,
    },
    /// Cold-resume one recoverable cell from its exact durable binding/session.
    CellRecover {
        #[arg(long = "run-id")]
        run_id: String,
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        model: Option<String>,
    },
    /// Check registry, pi session support, auth reference, and container-use.
    CellDoctor,
    /// Inspect SHUTTLE-owned subprocess resolution and bundle integrity.
    Tools {
        #[command(subcommand)]
        command: ToolsCmd,
    },
    /// Resolve git conflicts (W7 conflict plugin).
    Conflict {
        #[command(subcommand)]
        cmd: conflict::ConflictCmd,
    },
    /// Function-local refactors via native tree-sitter `locals` (py/js).
    /// Default is a dry-run preview; `--write` applies through the same
    /// all-or-nothing editcore gate as `shuttle rename`.
    Refactor {
        #[command(subcommand)]
        cmd: refactor::RefactorCmd,
    },
    /// Formal UI proposition evidence surface (U-series). `shuttle ui verify/couple/render/adopt`
    /// each read a closed-schema JSON request on stdin (or `--file`) and emit a
    /// three-valued verdict document. Evidence and judgment owner only (red-line #11).
    Ui {
        #[command(subcommand)]
        cmd: ui::UiCmd,
    },
    /// G7 · U2: the content-addressed **persistent symbol index**. `shuttle index`
    /// (default) refreshes the on-disk index over `root`, re-scanning only the
    /// files whose content changed since the last refresh (FNV-1a `(hash,size)`
    /// content addressing — a cache hit is a proof, not an mtime bet) and stamps
    /// it with the current `HEAD` oid. This is a shareable on-disk cache under
    /// `.shuttle/`, never a resident service (daemonless). `--status` is the fast
    /// read-only path: it reports whether the index may be trusted for `HEAD`
    /// without re-hashing, using the identical exact-match watermark rule T2's
    /// re-verify set uses — drift/cold/version-mismatch is `stale-index`, and a
    /// stale index is never silently reasoned off (§7 «绝不瞎猜»).
    Index {
        #[arg(long, default_value = ".")]
        root: String,
        /// Where the on-disk index lives (git-ignored `.shuttle/` by convention).
        #[arg(
            long = "cache-path",
            default_value = ".shuttle/symbols.index",
            conflicts_with = "refresh"
        )]
        cache_path: String,
        /// Read-only freshness check: report Fresh/Stale for the current HEAD
        /// without re-hashing or re-scanning. No write.
        #[arg(long, conflicts_with = "refresh")]
        status: bool,
        #[arg(long, conflicts_with = "refresh")]
        json: bool,
        /// Pure artifact mode: scan the exact clean `--head` checkout without
        /// reading or writing the legacy `.shuttle` cache.
        #[arg(long, requires = "head")]
        refresh: bool,
        /// Commit represented by the pure artifact. The worktree must be
        /// checked out cleanly at this commit.
        #[arg(long, requires = "refresh")]
        head: Option<String>,
        /// Caller-owned previous artifact to merge. Requires exact `--base`.
        #[arg(long, requires_all = ["refresh", "base"])]
        merge: Option<String>,
        /// Commit recorded by `--merge`; mismatch is a hard error.
        #[arg(long, requires_all = ["refresh", "merge"])]
        base: Option<String>,
        /// Caller-owned output path. Omit to emit canonical artifact JSON on
        /// stdout without filesystem residue.
        #[arg(long, requires = "refresh")]
        out: Option<String>,
        /// Bound the one-shot ctags scan. Timeout fails closed before any
        /// caller-owned output is replaced.
        #[arg(long, requires = "refresh")]
        timeout_ms: Option<u64>,
    },
}

#[derive(Subcommand)]
enum ToolsCmd {
    /// Emit deterministic machine-readable tool-bundle diagnostics.
    Doctor {
        #[arg(long)]
        json: bool,
    },
}

fn main() -> anyhow::Result<()> {
    // Rust 忽略 SIGPIPE,println! 遇下游提前关管(如 `| head`)会 panic。
    // CLI 惯例:恢复 SIGPIPE 默认行为(进程以信号 13 退出,而非 101 panic)。
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Apply {
            plan,
            root,
            dry_run,
            gate,
            enforce_scope,
            allowed_paths,
        } => run_apply(plan, root, dry_run, gate, enforce_scope, allowed_paths),
        Cmd::Locate {
            pattern,
            lang,
            root,
        } => run_locate(pattern, lang, root),
        Cmd::Repomap {
            budget,
            focus,
            root,
            no_cache,
            cache_path,
            scan_scope,
        } => run_repomap(budget, focus, root, !no_cache, cache_path, scan_scope),
        Cmd::Rename {
            from,
            to,
            root,
            write,
            include_refs,
            force,
            json,
        } => run_rename(from, to, root, write, include_refs, force, json),
        Cmd::References {
            symbol,
            root,
            kind,
            scan_scope,
            json,
        } => run_references(symbol, root, kind, scan_scope, json),
        Cmd::Symbols {
            file,
            query,
            root,
            kind,
            json,
        } => run_symbols(file, query, root, kind, json),
        Cmd::Defs {
            symbol,
            root,
            kind,
            json,
        } => run_defs(symbol, root, kind, json),
        Cmd::Impls {
            type_name,
            root,
            json,
        } => run_impls(type_name, root, json),
        Cmd::Callers { symbol, root, json } => run_callers(symbol, root, json),
        Cmd::Dataflow {
            seed,
            path,
            max_depth,
            json,
        } => run_dataflow(seed, path, max_depth, json),
        Cmd::Impact {
            changed,
            base,
            head,
            root,
            max_depth,
            tests,
            directed,
            telemetry,
            typed,
            contract,
            cochange,
            cochange_max_commits,
            json,
        } => run_impact(
            changed,
            base,
            head,
            root,
            max_depth,
            tests,
            directed,
            telemetry,
            typed,
            contract,
            cochange,
            cochange_max_commits,
            json,
        ),
        Cmd::Split {
            correct,
            derive_edges,
            plan,
            root,
            symbol_index,
            index_head,
            max_depth,
            max_fanout,
            telemetry,
            ledger,
            json,
        } => run_split(SplitRunOptions {
            correct,
            derive_edges,
            plan,
            root,
            symbol_index,
            index_head,
            max_depth,
            max_fanout,
            telemetry,
            ledger,
            json,
        }),
        Cmd::Goal {
            command,
            goal,
            plan,
            root,
            model,
            thinking,
            provider,
            base,
            head,
            max_depth,
            dry_run,
            parallel,
            advice_out,
            advice_version,
            symbol_footprint,
            observe,
            resume,
            telemetry,
            reconcile,
            diff,
            ledger,
            json,
        } => match command {
            Some(command) => goal_cli::run(*command),
            None => goal::run_goal(
                goal,
                plan,
                root,
                model,
                thinking,
                provider,
                base,
                head,
                max_depth,
                dry_run,
                parallel,
                advice_out,
                advice_version,
                symbol_footprint,
                observe,
                resume,
                telemetry,
                reconcile,
                diff,
                ledger,
                json,
            ),
        },
        Cmd::Verify {
            base,
            head,
            root,
            run,
            dry_run,
            binding,
            max_depth,
            partial,
            telemetry,
            json,
        } => verify::run_verify(
            base, head, root, run, dry_run, binding, max_depth, partial, telemetry, json,
        ),
        Cmd::Hover {
            symbol,
            root,
            kind,
            json,
        } => run_hover(symbol, root, kind, json),
        Cmd::Backends { root, json } => run_backends(root, json),
        Cmd::Calibrate { input, json } => run_calibrate(input, json),
        Cmd::Lsp { idle_secs } => run_lsp(idle_secs),
        Cmd::Typedef {
            path,
            line,
            col,
            symbol,
            type_hierarchy,
            root,
            json,
        } => run_typedef(path, line, col, symbol, type_hierarchy, root, json),
        Cmd::Codemod {
            rule_id,
            rules_dir,
            root,
            dry_run,
        } => run_codemod(rule_id, rules_dir, root, dry_run),
        Cmd::Rules { rules_dir } => run_rules(rules_dir),
        Cmd::ContextPack {
            tasks_dir,
            task_id,
            depends_on,
            touch_hint,
            git_base,
            symbol_index,
            index_head,
            write_symbols,
            budget,
            root,
            scan_scope,
        } => contextpack::run_context_pack(
            tasks_dir,
            task_id,
            depends_on,
            touch_hint,
            git_base,
            symbol_index,
            index_head,
            write_symbols,
            budget,
            root,
            scan_scope,
        ),
        Cmd::Run {
            prompt,
            model,
            thinking,
            provider,
            deep,
            exclude_tools,
            extension,
            root,
            no_session,
            passthrough,
        } => {
            // W4: layer built-in defaults < shuttle.toml < explicit CLI flags,
            // auto-inject the default provider + shuttle extension, and run the
            // presence-only provider-key preflight.
            let cfg = config::resolve(
                prompt,
                model,
                thinking,
                provider,
                exclude_tools,
                extension,
                root,
                deep,
                no_session,
                passthrough,
            );
            let tel = run::supervise(&cfg)?;
            // Telemetry goes to stderr as a single JSON line (eval B arm reads it);
            // pi's own stdout/stderr already flowed through (inherited stdio).
            eprintln!("{}", serde_json::to_string(&tel)?);
            std::process::exit(tel.exit_code);
        }
        Cmd::CellRun {
            binding,
            prompt,
            model,
            launch_bundle,
            receipt_out,
        } => {
            // The `--launch-bundle` flag selects the external-orchestrated path (lower
            // external contracts + emit a provider receipt). Without it, cell-run is
            // the direct-agent-launch path over a hand-written binding. The
            // stdin source is shared, so a bare `shuttle cell-run` (no flags) still
            // reads a binding from stdin; an external caller passes `--launch-bundle -`.
            if launch_bundle.is_some() || receipt_out.is_some() {
                cell_run::run_cell_from_external(launch_bundle, prompt, model, receipt_out)
            } else {
                cell_run::run_cell(binding, prompt, model)
            }
        }
        Cmd::CellObserve { run_id } => cell_run::observe_cell(&run_id),
        Cmd::CellCollect { run_id } => cell_run::collect_cell(&run_id),
        Cmd::CellUsage { run_id } => cell_run::usage_cell(&run_id),
        Cmd::CellCancel { run_id, grace_ms } => cell_run::cancel_cell(&run_id, grace_ms),
        Cmd::CellRecover {
            run_id,
            prompt,
            model,
        } => cell_run::recover_cell(&run_id, prompt, model),
        Cmd::CellDoctor => cell_run::doctor_cell(),
        Cmd::Tools { command } => match command {
            ToolsCmd::Doctor { json } => {
                if !json {
                    anyhow::bail!("shuttle tools doctor requires --json");
                }
                let resolver = shuttle_runtime_tools::ToolResolver::from_env()?;
                println!("{}", serde_json::to_string_pretty(&resolver.doctor())?);
                Ok(())
            }
        },
        Cmd::Conflict { cmd } => conflict::run(cmd),
        Cmd::Refactor { cmd } => refactor::run(cmd),
        Cmd::Ui { cmd } => ui::run(cmd),
        Cmd::Index {
            root,
            cache_path,
            status,
            json,
            refresh,
            head,
            merge,
            base,
            out,
            timeout_ms,
        } => run_index(IndexRunOptions {
            root,
            cache_path,
            status,
            json,
            pure_refresh: refresh,
            head,
            merge,
            base,
            out,
            timeout_ms,
        }),
    }
}

/// P0-2: resolve a write target through the scope resolver when enforcing.
/// No resolver == legacy behaviour (raw path). Any violation is surfaced as a
/// human reason so it can be attached to `ApplyStatus::Rejected`.
fn resolve_target(resolver: Option<&ScopedPathResolver>, path: &str) -> Result<PathBuf, String> {
    match resolver {
        Some(r) => r.resolve(path).map_err(|v| v.to_string()),
        None => Ok(PathBuf::from(path)),
    }
}

/// P0-2: a containment refusal. Not an anchor miss â the model must NOT retry.
fn rejected(path: &str, reason: String) -> ApplyOutcome {
    ApplyOutcome {
        path: path.to_string(),
        status: shuttle_protocol::ApplyStatus::Rejected { reason },
        match_tier: 0,
        message: Some("write path failed containment (P0-2)".into()),
    }
}

/// P1-3: real content-anchored insert. Locates `anchor` on exactly one line;
/// 0 hits -> AnchorMiss, >1 -> Ambiguous (mirrors search/replace honesty).
fn apply_insert(
    source: &str,
    anchor: &str,
    position: InsertPosition,
    content: &str,
) -> Result<String, shuttle_protocol::ApplyStatus> {
    let lines: Vec<&str> = source.lines().collect();
    let hits: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.contains(anchor))
        .map(|(i, _)| i)
        .collect();
    match hits.len() {
        0 => Err(shuttle_protocol::ApplyStatus::AnchorMiss),
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
        _ => Err(shuttle_protocol::ApplyStatus::Ambiguous),
    }
}

/// One evaluation pass over the plan's ops. `write=false` is a pure probe (no
/// disk mutation); `write=true` commits. Extracted so â¢ gate mode can run the
/// probe and the real write in ONE process â plan parsed once, resolver built
/// once â instead of the plugin spawning `shuttle apply` twice (kills W-C).
pub(crate) fn apply_pass(
    ops: &[EditOp],
    resolver_ref: Option<&ScopedPathResolver>,
    root: &str,
    write: bool,
) -> anyhow::Result<Vec<ApplyOutcome>> {
    let mut outcomes: Vec<ApplyOutcome> = Vec::with_capacity(ops.len());

    for op in ops {
        let outcome = match op {
            EditOp::SearchReplace {
                path,
                search,
                replace,
                line_hint,
            } => {
                // Gate FIRST â probe and real-write share the identical check.
                match resolve_target(resolver_ref, path) {
                    Err(reason) => rejected(path, reason),
                    Ok(target) => {
                        let source = std::fs::read_to_string(&target).unwrap_or_default();
                        let (new, mut outcome) = shuttle_editcore::apply_search_replace(
                            &source, search, replace, *line_hint,
                        );
                        outcome.path = path.clone();
                        if write && matches!(outcome.status, shuttle_protocol::ApplyStatus::Applied)
                        {
                            std::fs::write(&target, new)?;
                        }
                        outcome
                    }
                }
            }
            EditOp::CreateFile { path, content } => match resolve_target(resolver_ref, path) {
                Err(reason) => rejected(path, reason),
                Ok(target) => {
                    if write {
                        if let Some(parent) = target.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::write(&target, content)?;
                    }
                    ApplyOutcome {
                        path: path.clone(),
                        status: shuttle_protocol::ApplyStatus::Applied,
                        match_tier: 0,
                        message: Some("created".into()),
                    }
                }
            },
            EditOp::Insert {
                path,
                anchor,
                position,
                content,
            } => {
                // P1-3: real implementation (was a false "not implemented").
                match resolve_target(resolver_ref, path) {
                    Err(reason) => rejected(path, reason),
                    Ok(target) => {
                        let source = std::fs::read_to_string(&target).unwrap_or_default();
                        match apply_insert(&source, anchor, *position, content) {
                            Ok(new) => {
                                if write {
                                    std::fs::write(&target, new)?;
                                }
                                ApplyOutcome {
                                    path: path.clone(),
                                    status: shuttle_protocol::ApplyStatus::Applied,
                                    match_tier: 0,
                                    message: Some(format!("inserted {:?} anchor", position)),
                                }
                            }
                            Err(status) => ApplyOutcome {
                                path: path.clone(),
                                status,
                                match_tier: 0,
                                message: Some("insert anchor unresolved".into()),
                            },
                        }
                    }
                }
            }
            EditOp::Codemod { rule_id, paths } => {
                // P1-3: `paths` was silently ignored â that was a false success
                // (the caller believed scope narrowed but it didn't). Until the
                // codemod engine honours a path filter, refuse non-empty scope
                // explicitly instead of lying.
                if !paths.is_empty() {
                    ApplyOutcome {
                        path: rule_id.clone(),
                        status: shuttle_protocol::ApplyStatus::Rejected {
                            reason: "Codemod.paths scoping is not yet supported; \
                                     omit `paths` to run repo-wide, or split the rule"
                                .into(),
                        },
                        match_tier: 0,
                        message: Some("scoped codemod unsupported (P1-3)".into()),
                    }
                } else {
                    // Zero-token mechanical edit. probe counts sites; write rewrites.
                    let res: Result<usize, shuttle_codemod::CodemodError> = if !write {
                        shuttle_codemod::dry_run(rule_id, "rules", root).map(|m| m.len())
                    } else {
                        shuttle_codemod::apply(rule_id, "rules", root)
                    };
                    let (status, message) = match res {
                        Ok(n) => {
                            let verb = if !write { "would rewrite" } else { "rewrote" };
                            (
                                shuttle_protocol::ApplyStatus::Applied,
                                Some(format!("{verb} {n} site(s)")),
                            )
                        }
                        Err(e) => (shuttle_protocol::ApplyStatus::Error, Some(e.to_string())),
                    };
                    ApplyOutcome {
                        path: rule_id.clone(),
                        status,
                        match_tier: 0,
                        message,
                    }
                }
            }
        };
        outcomes.push(outcome);
    }

    Ok(outcomes)
}

fn run_apply(
    plan: Option<String>,
    root: String,
    dry_run: bool,
    gate: bool,
    enforce_scope: bool,
    allowed_paths: Vec<String>,
) -> anyhow::Result<()> {
    let raw = match plan {
        Some(path) => std::fs::read_to_string(path)?,
        None => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        }
    };
    let plan = EditPlan::parse(&raw)?;

    // P0-2: build the single shared resolver. editcore/codemod/conflict/insert
    // all route writes through THIS instance â never a per-tool copy (Â§2.1).
    let resolver: Option<ScopedPathResolver> = if enforce_scope {
        Some(ScopedPathResolver::new(&root, allowed_paths)?)
    } else {
        None
    };
    let resolver_ref = resolver.as_ref();

    // â¢ gate: probe every op then commit in ONE process â all-or-nothing. A
    // single unresolved op writes nothing (matches the plugin's old two-spawn
    // gate and P0-2 `Rejected` fail-closed). Non-gate keeps the legacy
    // per-op-write behaviour (dry_run => probe only).
    let outcomes = if gate {
        let probe = apply_pass(&plan.ops, resolver_ref, &root, false)?;
        let all_ok = probe
            .iter()
            .all(|o| matches!(o.status, shuttle_protocol::ApplyStatus::Applied));
        if all_ok && !dry_run {
            apply_pass(&plan.ops, resolver_ref, &root, true)?
        } else {
            probe
        }
    } else {
        apply_pass(&plan.ops, resolver_ref, &root, !dry_run)?
    };

    println!("{}", serde_json::to_string_pretty(&outcomes)?);
    Ok(())
}

fn run_locate(pattern: String, lang: Option<String>, root: String) -> anyhow::Result<()> {
    let hits = match lang {
        Some(l) => shuttle_locate::struct_locate(&pattern, &l, &root)?,
        None => shuttle_locate::text_locate(&pattern, &root)?,
    };
    for h in hits {
        println!("{}:{}:{}: {}", h.path, h.line, h.column, h.text);
    }
    Ok(())
}

fn run_repomap(
    budget: usize,
    focus: Vec<String>,
    root: String,
    cache: bool,
    cache_path: String,
    scan_scope: bool,
) -> anyhow::Result<()> {
    // One ctags scan feeds both rename and the repo-map (Â§7.5). --scan-scope (â¤)
    // narrows to the focus neighborhood; --cache (Â§7.5) reuses `.shuttle/symbols.cache`
    // and re-tags only changed files. Scope wins if both are set.
    let table = if scan_scope {
        shuttle_symbols::SymbolTable::scan_scoped(&root, &focus)?
    } else if cache {
        shuttle_symbols::SymbolTable::scan_cached(&root, &cache_path, &focus)?
    } else {
        shuttle_symbols::SymbolTable::scan(&root)?
    };
    let map = shuttle_symbols::repo_map(&table, budget, &focus)?;
    println!("{}", serde_json::to_string_pretty(&map)?);
    Ok(())
}

/// Parse a `--kind` string into a `SymbolKind` filter (None on unknown/empty).
fn parse_kind(k: Option<String>) -> Option<shuttle_symbols::SymbolKind> {
    use shuttle_symbols::SymbolKind::*;
    k.and_then(|k| {
        Some(match k.to_ascii_lowercase().as_str() {
            "function" | "func" | "fn" => Function,
            "method" => Method,
            "class" => Class,
            "struct" => Struct,
            "enum" => Enum,
            "variable" | "var" => Variable,
            "field" | "member" => Field,
            "constant" | "const" => Constant,
            "module" | "namespace" | "package" => Module,
            "service" => Service,
            "typedef" | "typealias" => TypeAlias,
            "section" | "table" => Section,
            "key" => Key,
            _ => return None,
        })
    })
}

/// Â§9.5 rename: dry-run diff + confidence by default; `--write` applies through
/// the same all-or-nothing gate as `shuttle apply --gate`. Low-confidence sites block
/// the write unless `--force`.
fn run_rename(
    from: String,
    to: String,
    root: String,
    write: bool,
    include_refs: bool,
    force: bool,
    json: bool,
) -> anyhow::Result<()> {
    let opts = shuttle_nav::QueryOptions::new(&root);
    let plan = shuttle_nav::plan_rename(&from, &to, &opts, include_refs)?;

    if !write {
        if json {
            println!("{}", serde_json::to_string_pretty(&plan)?);
        } else {
            print_rename_diff(&plan);
        }
        return Ok(());
    }

    // Reconciliation gate (SC-1): a def-only plan with known-but-excluded
    // reference sites would be a silently partial rename — refuse and name the
    // gaps, with the exact rerun that covers them.
    if plan.blocked {
        let mut msg = format!(
            "refusing to write a partial rename: planned {} / known {} sites.\n\
             excluded reference sites (add --include-refs to cover them):",
            plan.planned_count, plan.known_total
        );
        for g in &plan.excluded {
            let tag = confidence_tag(g.confidence);
            msg.push_str(&format!("\n  [{tag}] {}:{}", g.path, g.line));
            if let Some(s) = &g.snippet {
                msg.push_str(&format!("  {}", s.trim()));
            }
        }
        let low = plan
            .excluded
            .iter()
            .any(|g| matches!(g.confidence, shuttle_nav::Confidence::Low));
        msg.push_str(&format!(
            "\nre-run: shuttle rename --from {from} --to {to} --root {root} --write --include-refs{}",
            if low { " --force  (Low-confidence sites present)" } else { "" }
        ));
        anyhow::bail!("{msg}");
    }

    // §9.5 gate: refuse to write Low-confidence sites without --force.
    if plan.has_low_confidence() && !force {
        anyhow::bail!(
            "rename has Low-confidence sites (no AST grammar to disambiguate). \
             Re-run with --write --force to apply anyway, or inspect with --json."
        );
    }
    if plan.plan.ops.is_empty() {
        eprintln!("no rename sites for `{from}`");
        return Ok(());
    }

    // Apply via the shared editcore path, all-or-nothing (probe then commit).
    let probe = apply_pass(&plan.plan.ops, None, &root, false)?;
    let all_ok = probe
        .iter()
        .all(|o| matches!(o.status, shuttle_protocol::ApplyStatus::Applied));
    let outcomes = if all_ok {
        apply_pass(&plan.plan.ops, None, &root, true)?
    } else {
        probe
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&outcomes)?);
    } else {
        let n = outcomes
            .iter()
            .filter(|o| matches!(o.status, shuttle_protocol::ApplyStatus::Applied))
            .count();
        if all_ok {
            eprintln!("renamed {from} -> {to}: {n} site(s)");
        } else {
            anyhow::bail!("rename aborted: some sites did not resolve (all-or-nothing)");
        }
    }
    Ok(())
}

/// Human-readable rename preview: reconciliation header + scope line, then one
/// line per site with a confidence tag, then the excluded gaps (SC-2/SC-3).
fn print_rename_diff(plan: &shuttle_nav::RenamePlan) {
    println!(
        "planned {} / known {} sites",
        plan.planned_count, plan.known_total
    );
    if plan.blocked {
        println!(
            "  (blocked from --write: {} known site(s) excluded — add --include-refs)",
            plan.excluded.len()
        );
    }
    if plan.scope.narrow {
        let repo = plan.scope.repo_root.as_deref().unwrap_or("?");
        println!(
            "  (scope: scanning tracked files under {} only; sites outside it are NOT covered — repo root: {})",
            plan.scope.root, repo
        );
    }
    if plan.sites.is_empty() && plan.excluded.is_empty() {
        eprintln!("no rename sites");
        return;
    }
    for s in &plan.sites {
        let tag = confidence_tag(s.confidence);
        println!("[{tag}] {}:{}", s.path, s.line);
        println!("  - {}", s.old_line.trim_end());
        println!("  + {}", s.new_line.trim_end());
    }
    for g in &plan.excluded {
        let tag = confidence_tag(g.confidence);
        println!("[{tag}] {}:{}  (excluded)", g.path, g.line);
        if let Some(s) = &g.snippet {
            println!("    {}", s.trim_end());
        }
    }
}

/// Three-letter confidence tag shared by the plan preview and the gap list.
fn confidence_tag(c: shuttle_nav::Confidence) -> &'static str {
    match c {
        shuttle_nav::Confidence::High => "HIGH",
        shuttle_nav::Confidence::Medium => "MED ",
        shuttle_nav::Confidence::Low => "LOW ",
    }
}

/// Â§9.4 references.
fn run_references(
    symbol: String,
    root: String,
    kind: Option<String>,
    scan_scope: bool,
    json: bool,
) -> anyhow::Result<()> {
    let mut opts = shuttle_nav::QueryOptions::new(&root);
    opts.kind = parse_kind(kind);
    opts.scan_scope = scan_scope;
    let hits = shuttle_nav::references(&symbol, &opts)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        for h in &hits {
            let c = match h.confidence {
                shuttle_nav::Confidence::High => "HIGH",
                shuttle_nav::Confidence::Medium => "MED ",
                shuttle_nav::Confidence::Low => "LOW ",
            };
            let snip = h.snippet.as_deref().unwrap_or("").trim();
            println!("[{c}] {:?} {}:{}  {}", h.kind, h.path, h.line, snip);
        }
    }
    Ok(())
}

/// Â§9.6 symbols (documentSymbol via --file, workspace/symbol via --query).
fn run_symbols(
    file: Option<String>,
    query: Option<String>,
    root: String,
    kind: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let syms = match (file, query) {
        (Some(f), _) => shuttle_nav::document_symbols(&f)?,
        (None, q) => {
            let mut opts = shuttle_nav::QueryOptions::new(&root);
            opts.kind = parse_kind(kind);
            shuttle_nav::workspace_symbols(q.as_deref().unwrap_or(""), &opts)?
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&syms)?);
    } else {
        for s in &syms {
            let sig = s.signature.as_deref().unwrap_or("").trim();
            println!("{:?} {} {}:{}  {}", s.kind, s.name, s.path, s.line, sig);
        }
    }
    Ok(())
}

/// Â§9.6 defs.
fn run_defs(symbol: String, root: String, kind: Option<String>, json: bool) -> anyhow::Result<()> {
    let mut opts = shuttle_nav::QueryOptions::new(&root);
    opts.kind = parse_kind(kind);
    let syms = shuttle_nav::defs(&symbol, &opts)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&syms)?);
    } else {
        for s in &syms {
            let sig = s.signature.as_deref().unwrap_or("").trim();
            println!("{:?} {} {}:{}  {}", s.kind, s.name, s.path, s.line, sig);
        }
    }
    Ok(())
}

/// Shared NavHit printer with a confidence tag.
fn print_nav_hits(hits: &[shuttle_nav::NavHit], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(hits)?);
    } else {
        for h in hits {
            let c = match h.confidence {
                shuttle_nav::Confidence::High => "HIGH",
                shuttle_nav::Confidence::Medium => "MED ",
                shuttle_nav::Confidence::Low => "LOW ",
            };
            let snip = h
                .snippet
                .as_deref()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
                .trim();
            println!("[{c}] {:?} {}:{}  {}", h.kind, h.path, h.line, snip);
        }
    }
    Ok(())
}

/// Human-readable rendering of a `--typed` type-aware impact (JSON mode is the
/// machine contract). The narrowed hits print via the shared confidence-tagged
/// NavHit printer; every §7 degrade note and every provably-dropped directed
/// caller prints below, so a narrower answer is never mistaken for a proof that
/// the dropped callers are unaffected — they were *proven* type-independent,
/// while the degrades were *kept because we could not prove them*.
fn print_typed_impact(imp: &shuttle_refactor::TypedImpact) {
    if imp.hits.is_empty() {
        println!("(no type-dependent impact)");
    } else {
        let _ = print_nav_hits(&imp.hits, false);
    }
    if !imp.dropped.is_empty() {
        println!(
            "-- narrowed vs --directed: {} caller(s) proven type-independent --",
            imp.dropped.len()
        );
        for d in &imp.dropped {
            println!("  - {} @ {} ({})", d.symbol, d.path, d.reason);
        }
    }
    if imp.degraded {
        println!(
            "-- degraded (§7 «绝不瞎猜»): {} caller(s) kept — type proof unavailable --",
            imp.notes.len()
        );
        for n in &imp.notes {
            println!("  ! {n}");
        }
    }
}

/// Â§9.7 impls.
fn run_impls(type_name: String, root: String, json: bool) -> anyhow::Result<()> {
    let opts = shuttle_nav::QueryOptions::new(&root);
    let hits = shuttle_nav::impls(&type_name, &opts)?;
    print_nav_hits(&hits, json)
}

/// Â§9.7 callers (incoming only).
fn run_callers(symbol: String, root: String, json: bool) -> anyhow::Result<()> {
    let opts = shuttle_nav::QueryOptions::new(&root);
    let hits = shuttle_nav::callers(&symbol, &opts)?;
    print_nav_hits(&hits, json)
}

/// G6 · S3 dataflow — provable forward value-flow of a seed variable inside its
/// enclosing function. JSON mode is the machine contract; the human view prints
/// the tainted chain, then every sink and every unprovable frontier so an empty
/// flow is never mistaken for a proven "no flow" (§7). Exit code stays 0: the
/// envelope is the result a caller routes on, not a process failure.
fn run_dataflow(seed: String, path: String, max_depth: usize, json: bool) -> anyhow::Result<()> {
    let opts = shuttle_refactor::DataFlowOptions { max_depth };
    let flow = shuttle_refactor::dataflow(&seed, &path, &opts)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&flow)?);
    } else {
        print_dataflow(&flow);
    }
    Ok(())
}

fn confidence_word(c: shuttle_nav::Confidence) -> &'static str {
    match c {
        shuttle_nav::Confidence::High => "HIGH",
        shuttle_nav::Confidence::Medium => "MEDIUM",
        shuttle_nav::Confidence::Low => "LOW",
    }
}

fn print_dataflow(flow: &shuttle_refactor::DataFlow) {
    let fnname = flow.function.as_deref().unwrap_or("<anonymous>");
    println!(
        "dataflow of `{}` in {} (fn {})",
        flow.seed, flow.root_path, fnname
    );
    if flow.nodes.is_empty() {
        println!("  (no bindings — seed not found)");
    }
    for n in &flow.nodes {
        let arrow = if n.distance == 0 { "seed" } else { "copy" };
        println!(
            "  [{}] {} @ {}:{} ({}, d{})",
            arrow,
            n.name,
            n.path,
            n.line,
            confidence_word(n.confidence),
            n.distance
        );
    }
    if !flow.sinks.is_empty() {
        println!("-- sinks (value left the function — boundary, not followed) --");
        for s in &flow.sinks {
            println!(
                "  → {} flows into {}() @ {}:{}",
                s.value, s.callee, s.path, s.line
            );
        }
    }
    if !flow.unresolved.is_empty() {
        println!("-- unprovable stores (§7 «绝不瞎猜» —推不动即 None) --");
        for u in &flow.unresolved {
            println!(
                "  ? {} written through {} target @ {}:{} (not propagated)",
                u.value, u.kind, u.path, u.line
            );
        }
    }
    if flow.degraded {
        println!("-- degraded: analysis could not prove flow; see notes --");
    }
    for note in &flow.notes {
        println!("  ! {note}");
    }
}

/// M4 impact â blast radius of the symbols defined in `changed` files.
#[allow(clippy::too_many_arguments)]
fn run_impact(
    changed: Vec<String>,
    base: Option<String>,
    head: Option<String>,
    root: String,
    max_depth: usize,
    tests: bool,
    directed: bool,
    telemetry: bool,
    typed: bool,
    contract: bool,
    cochange: bool,
    cochange_max_commits: usize,
    json: bool,
) -> anyhow::Result<()> {
    let opts = shuttle_nav::QueryOptions::new(&root);
    // G7 · U1c co-change hint mode: `--cochange` surfaces the file-level
    // historical coupling of `--changed` as a REVIEW HINT that is structurally
    // separate from every mechanical closure — it is never a re-verify target
    // and never a walkable edge. It is mutually exclusive with the other truth
    // axes (contract / typed / land) so the hint is never confused for a proven
    // impact; it composes only with the plain `--changed` symbol closure below.
    if cochange {
        if contract || typed || base.is_some() || head.is_some() {
            anyhow::bail!(
                "shuttle impact --cochange is a review hint over --changed; drop --contract/--typed/--base/--head"
            );
        }
        if changed.is_empty() {
            anyhow::bail!("shuttle impact --cochange needs --changed <files...>");
        }
        let cfg = shuttle_symbols::cochange::CoChangeConfig::default();
        let hints =
            shuttle_nav::cochange::cochange_hints(&changed, &cfg, cochange_max_commits, &opts)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&hints)?);
        } else {
            print_cochange_hints(&hints);
        }
        return Ok(());
    }
    // G6 · S1 contract-graph mode: `--changed` names changed IDL files and we
    // answer the cross-language impact over provable ContractDirected edges.
    // This is a distinct truth axis from the mechanical symbol-graph closure,
    // so it short-circuits before the reverify / name-match paths. It never
    // lands and never guesses (#11, §7): degrades are surfaced, not laundered.
    if contract {
        if !changed.is_empty() && (base.is_some() || head.is_some()) {
            anyhow::bail!("shuttle impact --contract uses --changed <idl...>; drop --base/--head");
        }
        if changed.is_empty() {
            anyhow::bail!("shuttle impact --contract needs --changed <idl files...>");
        }
        let imp = shuttle_nav::contract::contract_impact(&changed, &opts)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&imp)?);
        } else {
            print_contract_impact(&imp);
        }
        return Ok(());
    }
    // G6 · S2 type-aware mode: refine the trusted directed closure to the
    // callers that provably type-depend on a changed symbol. A distinct
    // semantic-truth refinement (not a land-time set), so it short-circuits
    // before the reverify path. It never lands and never guesses a type it
    // can't see: unprovable callers are kept at Medium and surfaced, never
    // laundered into a clean narrowing (#11, §7).
    if typed {
        if contract {
            anyhow::bail!(
                "shuttle impact --typed and --contract are different truth axes; pick one"
            );
        }
        if base.is_some() || head.is_some() {
            anyhow::bail!("shuttle impact --typed uses --changed <files...>; drop --base/--head");
        }
        if changed.is_empty() {
            anyhow::bail!("shuttle impact --typed needs --changed <files...>");
        }
        let imp = shuttle_refactor::typed_impact(&changed, max_depth, tests, &opts)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&imp)?);
        } else {
            print_typed_impact(&imp);
        }
        return Ok(());
    }
    // T2 land-time mode: `--base <oid> --head <oid>` computes the minimal
    // re-verify set (changed units UNION reverse-dependency closure) with a
    // staleness watermark, emitted as a ToolOutcome. clap guarantees base/head
    // arrive together (each `requires` the other) and never with `--changed`.
    if let (Some(base), Some(head)) = (base, head) {
        let outcome = shuttle_nav::reverify::reverify_impact(&base, &head, max_depth, &opts)?;
        // T5: derive the reverify_size directly from the typed outcome and emit
        // it as a single JSON line to stderr. Pure projection (no re-derivation)
        // so the goal-decomposition ledger sees exactly what landed. stdout
        // stays the envelope; a None means the outcome carried no Reverify
        // detail (e.g. an early Error) and we simply skip the line.
        if telemetry {
            if let Some(size) = shuttle_protocol::reverify_size_of(&outcome) {
                let line = serde_json::json!({ "reverify_size": size });
                eprintln!("{}", serde_json::to_string(&line)?);
            }
        }
        if json {
            println!("{}", serde_json::to_string_pretty(&outcome)?);
        } else {
            print_reverify_outcome(&outcome);
        }
        // A Blocked/Error outcome is a machine-readable result the caller routes
        // on, not a process failure. Exit 0 so pipelines read the envelope.
        return Ok(());
    }
    if changed.is_empty() {
        anyhow::bail!(
            "shuttle impact needs either --changed <files...> or --base <oid> --head <oid>"
        );
    }
    let hits = shuttle_nav::impact_files(&changed, max_depth, tests, directed, &opts)?;
    print_nav_hits(&hits, json)
}

/// Human-readable rendering of a G7 co-change hint block (JSON mode is the
/// machine contract; this is the eyeball view). Co-change is a weak review
/// hint, so it prints under an explicit banner that names it as such and never
/// as an impact set. An insufficient-history run says so verbatim rather than
/// claim "no coupling" (§7 «绝不瞎猜»).
fn print_cochange_hints(h: &shuttle_nav::cochange::CoChangeHints) {
    println!("-- co-change hints (weak review signal — NOT the re-verify set) --");
    if h.insufficient_history {
        println!(
            "  insufficient-history: git history too shallow/short to establish co-change; no hint emitted (§7)"
        );
        return;
    }
    if h.is_empty() {
        println!("  (no above-threshold historical coupling for the changed files)");
        return;
    }
    for f in &h.files {
        if f.partners.is_empty() {
            continue;
        }
        println!("  {}", f.changed);
        for p in &f.partners {
            println!(
                "    ~ {}  (weight {:.2}, {} commit(s))",
                p.file, p.weight, p.commits
            );
        }
    }
}

/// Human-readable rendering of a `--contract` cross-language impact (JSON mode
/// is the machine contract; this is the eyeball view). Provable cross-language
/// hits print as HIGH `Caller` rows via the shared NavHit printer; every §7
/// degrade the walk hit prints below so an absent edge is never mistaken for a
/// proven "no impact".
fn print_contract_impact(imp: &shuttle_nav::contract::ContractImpact) {
    let hits = imp.to_nav_hits();
    if hits.is_empty() {
        println!("(no provable cross-language contract hits)");
    } else {
        // Reuse the confidence-tagged printer; contract hits are always HIGH.
        let _ = print_nav_hits(&hits, false);
    }
    if imp.degraded {
        println!(
            "-- degraded (§7 «绝不瞎猜»): {} note(s) --",
            imp.notes.len()
        );
        for n in &imp.notes {
            println!("  ! {n}");
        }
    }
}

/// Human-readable rendering of a T2 reverify `ToolOutcome` (JSON mode is the
/// machine contract; this is the eyeball view).
fn print_reverify_outcome(o: &shuttle_protocol::ToolOutcome) {
    println!("state: {}  reason: {}", o.state.as_str(), o.reason_code);
    if let Some(s) = &o.summary {
        println!("{s}");
    }
    if let shuttle_protocol::TypedDetail::Reverify(d) = &o.detail {
        println!(
            "base: {}  head: {}  index: {}  granularity: {}  degraded: {}",
            d.base_oid, d.head_oid, d.index_base_oid, d.granularity, d.degraded
        );
        for u in &d.reverify {
            println!(
                "  [{}] {}  (dist {}, {})",
                u.reason,
                u.path,
                u.distance,
                match u.confidence {
                    shuttle_protocol::OutcomeConfidence::High => "high",
                    shuttle_protocol::OutcomeConfidence::Medium => "medium",
                    shuttle_protocol::OutcomeConfidence::Low => "low",
                },
            );
        }
        println!("reverify_size: {}", d.reverify_size);
    }
    if let Some(a) = &o.next_action {
        println!("next: {a}");
    }
}

/// T1: `shuttle split --correct` â mechanical GoalGraph adjudication of an intent DAG.
struct SplitRunOptions {
    correct: bool,
    derive_edges: bool,
    plan: String,
    root: String,
    symbol_index: Option<String>,
    index_head: Option<String>,
    max_depth: usize,
    max_fanout: usize,
    telemetry: bool,
    ledger: Option<String>,
    json: bool,
}

fn run_split(options: SplitRunOptions) -> anyhow::Result<()> {
    let SplitRunOptions {
        correct,
        derive_edges,
        plan,
        root,
        symbol_index,
        index_head,
        max_depth,
        max_fanout,
        telemetry,
        ledger,
        json,
    } = options;
    if derive_edges {
        if telemetry || ledger.is_some() {
            anyhow::bail!("--telemetry/--ledger apply only to --correct");
        }
        let symbol_index = symbol_index
            .ok_or_else(|| anyhow::anyhow!("--derive-edges requires --symbol-index"))?;
        let index_head = index_head
            .or_else(|| shuttle_symbols::index::git_head_oid(&root))
            .ok_or_else(|| anyhow::anyhow!("--derive-edges requires --index-head outside git"))?;
        let index_head = resolve_git_revision(&root, &index_head).unwrap_or(index_head);
        let validated_plan = validate_split_derive_plan(&plan, &root, &symbol_index, &index_head)?;
        let detail = shuttle_nav::split::split_derive_edges(
            validated_plan.path(),
            &root,
            &symbol_index,
            &index_head,
            max_depth,
            max_fanout,
        )?;
        if json {
            println!("{}", serde_json::to_string_pretty(&detail)?);
        } else {
            println!(
                "derived edges={} weak={} tasks={} index={} truncated={} fallback_required={}",
                detail.hard_edges.len(),
                detail.weak_hints.len(),
                detail.tasks.len(),
                detail.index_status,
                detail.truncated,
                detail.fallback_required
            );
        }
        return Ok(());
    }
    if !correct {
        anyhow::bail!("shuttle split requires exactly one of --correct or --derive-edges");
    }
    let opts = shuttle_nav::QueryOptions::new(&root);
    let outcome = shuttle_nav::split::split_correct(&plan, &opts)?;
    // T5: project a `SplitFeedback` straight off the adjudicated outcome and
    // emit it as a single JSON line to stderr. It only counts what the
    // corrector already decided (agree/add/drop/conflict/unknown + provenance)
    // - never re-derives - so the downstream ledger measures how much the
    // mechanical graph corrected the planner. stdout keeps the envelope.
    if telemetry {
        if let Some(fb) = shuttle_protocol::SplitFeedback::from_outcome(&outcome) {
            eprintln!("{}", serde_json::to_string(&fb)?);
        }
    }
    // G9·D2: derive a replayable provenance ledger off the SAME adjudicated
    // outcome (never a re-derivation). The snapshot oid anchors every digest to
    // the exact tree the evidence was read at; a shallow/non-git tree yields an
    // empty oid (honest: unattributable snapshot, never a fabricated one). Only
    // the digest seed lands — the full evidence text stays out of the file.
    if let Some(path) = ledger.as_deref() {
        if let shuttle_protocol::TypedDetail::Split(detail) = &outcome.detail {
            let snapshot_oid = shuttle_symbols::index::git_head_oid(&root).unwrap_or_default();
            let led = shuttle_protocol::ProvenanceLedger::from_split(detail, snapshot_oid);
            std::fs::write(path, serde_json::to_string_pretty(&led)?)?;
        }
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        print_split_outcome(&outcome);
    }
    // A Blocked/Error outcome is not a process failure: it is a machine-readable
    // result the caller routes on. Exit 0 so pipelines read the envelope, not $?.
    Ok(())
}

/// Strictly validate the versioned derive request before handing it to nav.
/// v1 is the frozen compatibility input used by the evaluator. v2 accepts
/// canonical protocol identities and lowers only their name/path locator into
/// nav; identity validity and snapshot binding are checked first.
fn validate_split_derive_plan(
    plan_path: &str,
    root: &str,
    symbol_index: &str,
    index_head: &str,
) -> anyhow::Result<ValidatedDerivePlan> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacySeed {
        name: String,
        path: String,
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacyTask {
        id: String,
        #[serde(default)]
        intent: String,
        #[serde(default)]
        write_files: Vec<String>,
        #[serde(default)]
        write_symbols: Vec<LegacySeed>,
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacyPlan {
        version: String,
        #[serde(default)]
        goal: String,
        tasks: Vec<LegacyTask>,
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct TypedTask {
        id: String,
        #[serde(default)]
        intent: String,
        #[serde(default)]
        write_symbols: Vec<shuttle_protocol::SymbolIdentity>,
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct TypedPlan {
        version: String,
        #[serde(default)]
        goal: String,
        tasks: Vec<TypedTask>,
    }

    fn validate_task_ids<'a>(ids: impl Iterator<Item = &'a str>) -> anyhow::Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        for id in ids {
            if id.trim() != id || id.is_empty() || !seen.insert(id) {
                anyhow::bail!("derive plan task ids must be non-empty, trimmed, and unique");
            }
        }
        if seen.is_empty() {
            anyhow::bail!("derive plan must contain at least one task");
        }
        Ok(())
    }

    let bytes = std::fs::read(plan_path)?;
    if bytes.len() > 4 * 1024 * 1024 {
        anyhow::bail!("derive plan exceeds the 4MiB input limit");
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("derive plan requires a string version"))?;
    match version {
        "split-derive/v1" => {
            let parsed: LegacyPlan = serde_json::from_value(value)?;
            validate_task_ids(parsed.tasks.iter().map(|task| task.id.as_str()))?;
            if parsed
                .tasks
                .iter()
                .any(|task| task.write_files.is_empty() && task.write_symbols.is_empty())
            {
                anyhow::bail!("every derive task requires explicit write evidence");
            }
            // Read every field so schema changes cannot accidentally become a
            // permissive parse merely because nav has not consumed it yet.
            let _ = (&parsed.version, &parsed.goal);
            for task in &parsed.tasks {
                let _ = (&task.intent, &task.write_files);
                for seed in &task.write_symbols {
                    if seed.name.trim().is_empty() || seed.path.trim().is_empty() {
                        anyhow::bail!("legacy write_symbols require non-empty name and path");
                    }
                }
            }
            Ok(ValidatedDerivePlan::Borrowed(plan_path.to_string()))
        }
        "split-derive/v2" => {
            let parsed: TypedPlan = serde_json::from_value(value)?;
            validate_task_ids(parsed.tasks.iter().map(|task| task.id.as_str()))?;
            let mut repo_bases = std::collections::BTreeSet::new();
            for task in &parsed.tasks {
                if task.write_symbols.is_empty() {
                    anyhow::bail!("every typed derive task requires write_symbols");
                }
                for identity in &task.write_symbols {
                    identity.validate()?;
                    repo_bases.insert((identity.repo_id.as_str(), identity.base_oid.as_str()));
                }
            }
            if repo_bases.len() != 1 {
                anyhow::bail!("typed derive plan must bind exactly one repository base");
            }
            let (_, base_oid) = repo_bases.into_iter().next().expect("one binding");
            if base_oid != index_head {
                anyhow::bail!(
                    "typed derive identity base_oid does not match --index-head: {base_oid} != {index_head}"
                );
            }
            if let shuttle_symbols::index::ArtifactRead::Fresh(table) =
                shuttle_symbols::index::read_artifact_for_revision(root, symbol_index, index_head)
            {
                for task in &parsed.tasks {
                    for identity in &task.write_symbols {
                        let matching = table
                            .symbols
                            .iter()
                            .filter_map(|symbol| {
                                canonical_symbol_identity(
                                    symbol,
                                    root,
                                    &identity.repo_id,
                                    index_head,
                                )
                            })
                            .filter(|actual| actual == identity)
                            .count();
                        if matching != 1 {
                            anyhow::bail!(
                                "typed write symbol {} is not an exact unique member of the supplied index",
                                identity.symbol_id
                            );
                        }
                    }
                }
            }
            let lowered = shuttle_nav::split::DerivePlan {
                version: parsed.version,
                goal: parsed.goal,
                tasks: parsed
                    .tasks
                    .into_iter()
                    .map(|task| shuttle_nav::split::DeriveTaskInput {
                        id: task.id,
                        intent: task.intent,
                        write_files: Vec::new(),
                        write_symbols: task
                            .write_symbols
                            .into_iter()
                            .map(|identity| shuttle_nav::split::DeriveSymbolSeed {
                                name: identity.name,
                                path: identity.normalized_path,
                            })
                            .collect(),
                    })
                    .collect(),
            };
            ValidatedDerivePlan::temporary(&lowered)
        }
        other => anyhow::bail!("unsupported derive plan version: {other}"),
    }
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
            .strip_prefix(std::fs::canonicalize(root).ok()?)
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

fn resolve_git_revision(root: &str, revision: &str) -> Option<String> {
    let commit = format!("{revision}^{{commit}}");
    let binary = shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git).ok()?;
    let output = std::process::Command::new(binary)
        .args(["-C", root, "rev-parse", "--verify", &commit])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

enum ValidatedDerivePlan {
    Borrowed(String),
    Temporary(PathBuf),
}

impl ValidatedDerivePlan {
    fn temporary(plan: &shuttle_nav::split::DerivePlan) -> anyhow::Result<Self> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "shuttle-split-derive-{}.{}.json",
            std::process::id(),
            nonce
        ));
        std::fs::write(&path, serde_json::to_vec(plan)?)?;
        Ok(Self::Temporary(path))
    }

    fn path(&self) -> &str {
        match self {
            Self::Borrowed(path) => path,
            Self::Temporary(path) => path.to_str().expect("temporary path is UTF-8"),
        }
    }
}

impl Drop for ValidatedDerivePlan {
    fn drop(&mut self) {
        if let Self::Temporary(path) = self {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Human-readable rendering of a split `ToolOutcome` (the JSON mode is the
/// machine contract; this is the eyeball view).
fn print_split_outcome(o: &shuttle_protocol::ToolOutcome) {
    println!("state: {}  reason: {}", o.state.as_str(), o.reason_code);
    if let Some(s) = &o.summary {
        println!("{s}");
    }
    if let shuttle_protocol::TypedDetail::Split(d) = &o.detail {
        if !d.goal.is_empty() {
            println!("goal: {}", d.goal);
        }
        for e in &d.edges {
            println!(
                "  [{}] {} -> {}  ({}, {})  {}",
                e.verdict.as_str(),
                e.from,
                e.to,
                match e.confidence {
                    shuttle_protocol::OutcomeConfidence::High => "high",
                    shuttle_protocol::OutcomeConfidence::Medium => "medium",
                    shuttle_protocol::OutcomeConfidence::Low => "low",
                },
                e.source.as_str(),
                e.evidence,
            );
        }
        println!(
            "added: {}  dropped: {}  review: {}",
            d.added, d.dropped, d.review_surface
        );
    }
    if !o.whats_missing.is_empty() {
        println!("needs review:");
        for m in &o.whats_missing {
            println!("  - {m}");
        }
    }
    if let Some(a) = &o.next_action {
        println!("next: {a}");
    }
}

/// Â§9.7 hover.
fn run_hover(symbol: String, root: String, kind: Option<String>, json: bool) -> anyhow::Result<()> {
    let mut opts = shuttle_nav::QueryOptions::new(&root);
    opts.kind = parse_kind(kind);
    let hits = shuttle_nav::hover(&symbol, &opts)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        for h in &hits {
            println!("{}:{}", h.path, h.line);
            if let Some(s) = &h.snippet {
                for l in s.lines() {
                    println!("  {l}");
                }
            }
        }
    }
    Ok(())
}

fn run_codemod(
    rule_id: String,
    rules_dir: String,
    root: String,
    dry_run: bool,
) -> anyhow::Result<()> {
    if dry_run {
        // Preview: emit the structured match list as JSON.
        let matches = shuttle_codemod::dry_run(&rule_id, &rules_dir, &root)?;
        println!("{}", serde_json::to_string_pretty(&matches)?);
    } else {
        let n = shuttle_codemod::apply(&rule_id, &rules_dir, &root)?;
        eprintln!("rule {rule_id}: rewrote {n} site(s)");
    }
    Ok(())
}

fn run_rules(rules_dir: String) -> anyhow::Result<()> {
    for id in shuttle_codemod::list_rules(&rules_dir)? {
        println!("{id}");
    }
    Ok(())
}

/// Â§9.8 backends â print grammar availability per language.
struct IndexRunOptions {
    root: String,
    cache_path: String,
    status: bool,
    json: bool,
    pure_refresh: bool,
    head: Option<String>,
    merge: Option<String>,
    base: Option<String>,
    out: Option<String>,
    timeout_ms: Option<u64>,
}

fn run_index(options: IndexRunOptions) -> anyhow::Result<()> {
    let IndexRunOptions {
        root,
        cache_path,
        status,
        json,
        pure_refresh,
        head,
        merge,
        base,
        out,
        timeout_ms,
    } = options;
    if pure_refresh {
        let head = head.expect("clap requires --head with --refresh");
        let timeout = timeout_ms.map(std::time::Duration::from_millis);
        let (artifact, stats) = shuttle_symbols::index::refresh_artifact_with_timeout(
            &root,
            &head,
            merge.as_deref(),
            base.as_deref(),
            timeout,
        )?;
        if let Some(path) = out {
            shuttle_symbols::index::write_artifact_atomic(&path, &artifact)?;
        } else {
            use std::io::Write as _;
            std::io::stdout().write_all(&artifact)?;
        }
        eprintln!(
            "pure-index files={} reused={} rebuilt={} rebuilt_paths={} head={}",
            stats.total_files,
            stats.reused,
            stats.rescanned,
            serde_json::to_string(&stats.rescanned_paths)?,
            head
        );
        return Ok(());
    }

    // Resolve HEAD once: the watermark the whole freshness contract hangs on.
    // Outside git (or unborn branch) this is empty, and every freshness check
    // honestly degrades to stale rather than claim a proof we cannot make.
    let head_oid = shuttle_symbols::index::git_head_oid(&root).unwrap_or_default();

    if status {
        // Fast read-only path: trust the on-disk watermark, never re-hash.
        let freshness = shuttle_symbols::index::load_status(&cache_path, &head_oid);
        if json {
            let v = match &freshness {
                shuttle_symbols::index::IndexFreshness::Fresh {
                    index_base_oid,
                    files,
                } => serde_json::json!({
                    "status": "fresh",
                    "index_base_oid": index_base_oid,
                    "files": files,
                }),
                shuttle_symbols::index::IndexFreshness::Stale {
                    index_base_oid,
                    head_oid,
                } => serde_json::json!({
                    "status": "stale-index",
                    "index_base_oid": index_base_oid,
                    "head_oid": head_oid,
                }),
            };
            println!("{}", serde_json::to_string_pretty(&v)?);
        } else {
            match &freshness {
                shuttle_symbols::index::IndexFreshness::Fresh {
                    index_base_oid,
                    files,
                } => {
                    println!("fresh  index_base_oid={index_base_oid}  files={files}");
                }
                shuttle_symbols::index::IndexFreshness::Stale {
                    index_base_oid,
                    head_oid,
                } => {
                    let recorded = if index_base_oid.is_empty() {
                        "<none>"
                    } else {
                        index_base_oid.as_str()
                    };
                    let want = if head_oid.is_empty() {
                        "<no-head>"
                    } else {
                        head_oid.as_str()
                    };
                    println!("stale-index  index_base_oid={recorded}  head_oid={want}");
                    println!("hint: run `shuttle index` to refresh before reasoning off it (§7).");
                }
            }
        }
        return Ok(());
    }

    // Refresh path: rebuild only the changed delta and re-stamp the watermark.
    let (table, stats) = shuttle_symbols::index::refresh(&root, &cache_path, &head_oid)?;
    if json {
        let v = serde_json::json!({
            "index_base_oid": stats.index_base_oid,
            "total_files": stats.total_files,
            "cache_hits": stats.cache_hits,
            "rebuilt": stats.rebuilt,
            "dropped": stats.dropped,
            "symbols": table.symbols.len(),
            "cache_path": cache_path,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        let oid = if stats.index_base_oid.is_empty() {
            "<no-head>"
        } else {
            stats.index_base_oid.as_str()
        };
        println!(
            "refreshed {cache_path}  index_base_oid={oid}  files={}  hits={}  rebuilt={}  dropped={}  symbols={}",
            stats.total_files,
            stats.cache_hits,
            stats.rebuilt,
            stats.dropped,
            table.symbols.len(),
        );
    }
    Ok(())
}

fn run_lsp(idle_secs: u64) -> anyhow::Result<()> {
    // Daemonless LSP server (Â§9.9·D1): stdio transport only, spawn-per-session,
    // idle-exit. serve_stdio touches ONLY the inherited stdin/stdout pipes â it
    // never opens a socket or binds a port. Returns (process exits) on `exit`
    // or EOF.
    let idle = std::time::Duration::from_secs(idle_secs);
    shuttle_lsp_bridge::server::serve_stdio(idle)?;
    Ok(())
}

/// §9.9 tier-3 — `shuttle typedef`: resolve the type of an expression at a
/// position through a REAL external language server (goto type-definition, or
/// the type hierarchy with `--type-hierarchy`).
///
/// Contract with the rest of shuttle (§7 「绝不瞎猜」 + §9.9):
/// * The language is routed purely from the file extension. An extension with
///   no tier-3 server maps to nothing — we say so and exit 0 with zero hits,
///   never guessing a server.
/// * The `real-lsp` feature is OFF by default, so the stock binary answers with
///   an honest "compiled LSP-free" note and a rebuild hint — not an error and
///   not a fabricated hit.
/// * A missing server binary, an unreadable file, or a JSON-RPC failure are all
///   reported honestly; an empty/anonymous server result yields zero hits.
///
/// Every hit that DOES come back is already tagged "via external LSP" and
/// carries `Backend::Lsp` + `Confidence::High` from the bridge, so the planner
/// can never confuse it with shuttle's static ladder.
fn run_typedef(
    path: String,
    line: usize,
    col: usize,
    symbol: String,
    type_hierarchy: bool,
    root: String,
    json: bool,
) -> anyhow::Result<()> {
    // Route the language from the file extension only — never from content, so
    // an unknown extension degrades to "no tier-3 server" rather than a guess.
    let normalized = path.replace('\\', "/");
    let normalized = normalized.strip_prefix("./").unwrap_or(&normalized);
    let lang = match shuttle_symbols::lang_for_path(normalized) {
        Some(l) => l,
        None => {
            let msg = format!(
                "shuttle typedef: no language could be determined for `{path}` — tier-3 type resolution needs a known source extension"
            );
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "op": if type_hierarchy { "type-hierarchy" } else { "type-definition" },
                        "path": path,
                        "server": serde_json::Value::Null,
                        "available": shuttle_lsp_bridge::real_lsp_enabled(),
                        "hits": [],
                        "note": msg,
                    })
                );
            } else {
                eprintln!("{msg}");
            }
            return Ok(());
        }
    };
    let server = match shuttle_lsp_bridge::LspServer::for_lang(lang) {
        Some(s) => s,
        None => {
            let msg = format!(
                "shuttle typedef: language `{lang}` has no tier-3 external server in shuttle's bridge (supported: rust, typescript/tsx/javascript, python)"
            );
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "op": if type_hierarchy { "type-hierarchy" } else { "type-definition" },
                        "path": path,
                        "language": lang,
                        "server": serde_json::Value::Null,
                        "available": shuttle_lsp_bridge::real_lsp_enabled(),
                        "hits": [],
                        "note": msg,
                    })
                );
            } else {
                eprintln!("{msg}");
            }
            return Ok(());
        }
    };

    // Resolve --root to an ABSOLUTE path so the bridge emits a valid
    // `file://<absolute>` document URI (real language servers reject a
    // relative file URL). Canonicalisation is an impure step, so it lives here
    // in the CLI shell — the transport-generic conversation stays pure. If the
    // root cannot be canonicalised we fall back to cwd-join, still absolute.
    let abs_root = std::fs::canonicalize(&root)
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|c| {
                    if root == "." || root.is_empty() {
                        c.to_str().map(|s| s.to_string())
                    } else if std::path::Path::new(&root).is_absolute() {
                        Some(root.clone())
                    } else {
                        c.join(&root).to_str().map(|s| s.to_string())
                    }
                })
                .unwrap_or_else(|| root.clone())
        });

    let op = if type_hierarchy {
        shuttle_lsp_bridge::LspOp::TypeHierarchy
    } else {
        shuttle_lsp_bridge::LspOp::TypeDefinition
    };
    let req = shuttle_lsp_bridge::LspRequest {
        server,
        root: abs_root,
        path: normalized.to_string(),
        line,
        col,
        symbol: symbol.clone(),
        op,
    };

    match shuttle_lsp_bridge::resolve(&req) {
        Ok(hits) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "op": if type_hierarchy { "type-hierarchy" } else { "type-definition" },
                        "path": path,
                        "language": lang,
                        "server": server.command(),
                        "available": true,
                        "hits": hits,
                    })
                );
            } else if hits.is_empty() {
                eprintln!(
                    "shuttle typedef: {} — no result at {path}:{line}:{col} (the server returned nothing; shuttle never fabricates a position)",
                    server.command()
                );
            } else {
                for h in &hits {
                    println!("{}:{}", h.path, h.line);
                    if let Some(s) = &h.snippet {
                        for l in s.lines() {
                            println!("  {l}");
                        }
                    }
                }
            }
            Ok(())
        }
        Err(shuttle_lsp_bridge::BridgeError::Disabled) => {
            // The stock binary is intentionally LSP-free. This is an honest
            // capability boundary, not a failure — exit 0 with a rebuild hint.
            let note = "shuttle was compiled LSP-free (tier-3 external servers are off by default); rebuild shuttle-lsp-bridge with `--features real-lsp` to enable `shuttle typedef`";
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "op": if type_hierarchy { "type-hierarchy" } else { "type-definition" },
                        "path": path,
                        "language": lang,
                        "server": server.command(),
                        "available": false,
                        "hits": [],
                        "note": note,
                    })
                );
            } else {
                eprintln!("{note}");
            }
            Ok(())
        }
        Err(shuttle_lsp_bridge::BridgeError::ServerMissing(bin)) => {
            let note = format!(
                "shuttle typedef: language server `{bin}` for `{lang}` was not found on PATH — install it to enable tier-3 type resolution"
            );
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "op": if type_hierarchy { "type-hierarchy" } else { "type-definition" },
                        "path": path,
                        "language": lang,
                        "server": bin,
                        "available": false,
                        "hits": [],
                        "note": note,
                    })
                );
                Ok(())
            } else {
                Err(anyhow::anyhow!(note))
            }
        }
        Err(shuttle_lsp_bridge::BridgeError::Query(e)) => {
            Err(anyhow::anyhow!("shuttle typedef: LSP query failed: {e}"))
        }
    }
}

fn run_calibrate(input: String, json: bool) -> anyhow::Result<()> {
    // Pure projection: read the execution-truth samples, calibrate, emit the
    // verdict. No repo access, no I/O beyond the one input file (§4 layering).
    let raw = std::fs::read_to_string(&input)?;
    let cin: shuttle_protocol::CalibrationInput = serde_json::from_str(&raw)?;
    let out = shuttle_protocol::calibrate(&cin);
    if json {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!(
            "tier    floor   verdict\nhigh    {:<7.3} {}\nmedium  {:<7.3} {}\nlow     {:<7.3} {}",
            out.high_floor,
            verdict_label(&out.high),
            out.medium_floor,
            verdict_label(&out.medium),
            out.low_floor,
            verdict_label(&out.low),
        );
    }
    Ok(())
}

/// One-line human label for a tier verdict (the `--json` path carries the full
/// machine-readable rationale).
fn verdict_label(v: &shuttle_protocol::TierVerdict) -> String {
    match v {
        shuttle_protocol::TierVerdict::Adjusted {
            from,
            to,
            observed_rate,
            sample,
        } => format!("adjusted {from:.3}â{to:.3} (observed {observed_rate:.3}, n={sample})"),
        shuttle_protocol::TierVerdict::FrozenInsufficient { default, sample } => {
            format!("frozen {default:.3} (insufficient sample n={sample})")
        }
        shuttle_protocol::TierVerdict::PinnedProvable {
            floor,
            observed_rate,
        } => format!("pinned {floor:.3} (provable; observed {observed_rate:.3})"),
    }
}

fn run_backends(root: String, json: bool) -> anyhow::Result<()> {
    let probe = shuttle_nav::backend::BackendProbe::probe(&root);
    let report = probe.report();
    if json {
        let rows: Vec<serde_json::Value> = report
            .iter()
            .map(|(lang, status, ast)| {
                serde_json::json!({
                    "lang": lang,
                    "status": format!("{status:?}"),
                    "ast_available": ast,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        println!("{:<12} {:<20} AST", "LANG", "STATUS");
        for (lang, status, ast) in &report {
            let tag = if *ast { "yes" } else { "no" };
            println!("{lang:<12} {:<20} {tag}", format!("{status:?}"));
        }
    }
    Ok(())
}

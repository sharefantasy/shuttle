# Changelog

All notable changes to this project are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] — 2026-10-28

First public release. Derived from six months of stable internal use; the
earlier development history is not part of this repository.

### Added
- Unified GitHub, Gitea, and Gitee release pipelines for four native binary
  archives, deterministic checksums and CycloneDX SBOM, plus digest-bound
  multi-architecture runtime images with immutable digest receipts.
- **Digest-bound parallel Goal advice** (`shuttle goal --parallel --advice-out`):
  emits the stable `shuttle_goal_parallel_advice/v1` contract from an immutable Git
  tree, binding source intent, base OID, symbol index, correction digest,
  deterministic batches, serialization reasons, and degraded status. Advice is
  effect-free and requires explicit consumer adoption.
- **Governed Goal facade adapter**: `shuttle goal` can route `goal:external:*` reads and
  lifecycle mutations through an explicitly configured, one-shot
  `goal-control --json` process. The v1 transport validates request identity,
  schema, bounded streams, deadlines, and typed result/conflict/error outcomes;
  it never falls back to or dual-writes the local store. Governed actions
  commit intent before the shared caged `cell-run`, heartbeat controller CAS,
  retain bounded result evidence, and replay settled effects without a
  second provider invocation.
- **Explicit Goal backend migration**: `seal -> export -> import -> activate ->
  transfer` binds the target to the sealed source revision and bundle digest;
  only a verified active target permits the source to enter `transferred`.
- **Typed automatic condition updates**: action bundles may name an explicit
  `satisfies` gate set. Successful results update only known gates, and
  verification gates require independent command evidence before atomic
  completion.
- **Durable bounded Goals** (`shuttle goal run`): authority-free action plans are
  rebound against the latest Goal/attachment revisions before every action and
  stop on action, token, wall-time, provider-failure, and no-progress budgets.
  Run plans must end in a disposable, read-only independent verifier.
- **Caged verification command gate**: the trusted Goal driver executes exact
  pre-approved argv without a shell, after Pi exits, using secret-environment
  scrubbing, disposable Git workspace snapshots, bounded output/deadlines, and
  content-addressed verifier evidence.
- **Symbol dependency graph & impact analysis** (`shuttle-symbols::graph`, `shuttle-nav`,
  `shuttle impact`): native symbol-level reference graph with reverse-dependency
  closure (blast-radius). `shuttle impact --changed <files> [--max-depth N]
  [--tests] [--json]` reports which symbols / files / tests a change reaches.
  Daemonless — every query is a one-shot pure call (cold start ~5–28 ms).
- **Context-pack symbol-level read/write sets**: opt-in (`SHUTTLE_GRAPH_IMPACT`)
  enrichment adding symbol edges, reverse closure, and mechanically-derived
  `touch_hint`. Legacy output is byte-for-byte unchanged when off.
- **Merge-conflict resolution engine** (`shuttle-conflict`): L1 structural /
  L2 tree-sitter-scope-aware disambiguation with a non-skippable verify gate.
- **`customLanguages` extension**: Thrift and Bash support in the symbol/nav
  ladder via configurable ctags/ast-grep backends.

### Changed
- **Upstream orchestrator decoupling**: the caged-run path now sits behind an
  `UpstreamOrchestrator` seam. A stock build has zero orchestrator dependency;
  an optional launch bundle can drive the same code path when present.

## [0.1.0] — Initial public release

### Added
- **Edit apply engine** (`shuttle-editcore`): 4-tier fuzzy, content-anchored
  search/replace ladder (exact → whitespace-normalized → similarity → miss).
  Only a total miss is expensive. Never uses line-numbered diffs.
- **Locate** (`shuttle-locate`): ripgrep / ast-grep identifier and pattern lookup.
- **Symbol table & repo-map** (`shuttle-symbols`): ctags-backed symbol index and
  repository map, daemonless.
- **Navigation** (`shuttle-nav`): references, definitions, callers, implementations,
  rename (dry-run / `--write`), and hover with confidence scoring.
- **AST-precise refactors** (`shuttle-refactor`, via tree-sitter): extract/inline
  method, extract/inline variable & constant, rename-local, change-signature,
  move-function (with optional import synthesis), introduce/inline/remove
  parameter, extract-to-new-file, implement-members, generate-boilerplate,
  create-from-usage, scaffold-test, new-module-wiring, and transactional batch
  refactors. Multi-line signature and class-method/`self`-receiver aware.
- **Codemods** (`shuttle-codemod`): ast-grep rule-driven, zero-token mechanical
  edits with an external rule directory and injectable `*.recipe.json`.
- **Declarative composite recipes** and tree-sitter query point-selection
  primitives.
- **Uniform tool outcome envelope** (`ToolOutcome`) with machine-readable
  confidence / warnings, plus fallback telemetry across refactor/nav/recipe.
- **Model routing & assembly-line orchestration** (`shuttle-protocol`): criteria
  table, dispatch layer, budget-aware context closure, dual-receipt audit, and
  a mechanical-task whitelist.
- **Caged execution cell** (`shuttle cell-run --binding`): fail-closed write
  boundary from a hand-written `cell_binding/v1` envelope; no external
  orchestrator required.
- **Off-by-default LSP bridge** (`shuttle-lsp-bridge`): tier-3 type-level
  capabilities isolated behind a feature flag.
- **`shuttle` CLI**: host-agent entry point wiring all capabilities together.

[Unreleased]: https://codeup.aliyun.com/67b894ecf6e36ba63f99da60/growth/shuttle/compare/v0.2.0...HEAD
[0.2.0]: https://codeup.aliyun.com/67b894ecf6e36ba63f99da60/growth/shuttle/releases/tag/v0.2.0
[0.1.0]: https://codeup.aliyun.com/67b894ecf6e36ba63f99da60/growth/shuttle/releases/tag/v0.1.0

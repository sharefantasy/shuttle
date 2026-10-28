# Contributing to shuttle

Thanks for taking the time to contribute. shuttle is a small, latency-sensitive
execution kernel, so the bar for changes is less about volume and more about
predictability: every patch must keep the hot path fast and the contracts
explicit.

## What belongs here

shuttle is **not** an agent loop. It is the execution layer a host agent calls
into: locate, navigate, refactor, apply edits, and drive a bounded Goal. Changes
that add planning, model calls, or prompt logic to this repository are usually
out of scope.

Good first contributions are usually:

- a new codemod rule under `rules/`
- a missing refactor case, or a failing edge case with a regression test
- accuracy or latency improvements in `shuttle-symbols` / `shuttle-editcore`
- documentation fixes, especially reproducible bug reports

Please open an issue before starting anything that touches wire contracts,
the `cell-run` write boundary, or the `goal` journal.

## Prerequisites

| Requirement | Notes |
|---|---|
| Rust 1.90+ | `rust-version` in `Cargo.toml`; CI uses the stable channel |
| `rustfmt`, `clippy` | declared in `rust-toolchain.toml` |
| macOS or Linux | Windows is not supported |
| `git` | required by `impact --changed` and by the diff paths |
| `universal-ctags`, `ast-grep` | **optional**; they upgrade the analysis ladder, and shuttle degrades rather than fails without them |

```bash
git clone https://github.com/fankainang/shuttle
cd shuttle
cargo build --release
```

The release profile uses fat LTO and a single codegen unit, so the first build
is slow. `cargo build` (debug) is fine for day-to-day work.

## Repository layout

| Crate | Responsibility |
|---|---|
| `shuttle-protocol` | wire contracts: `EditPlan`, `EditOp`, outcomes, routing |
| `shuttle-locate` | ripgrep / ast-grep location wrappers |
| `shuttle-symbols` | ctags symbol table, repo map, symbol-level reference graph |
| `shuttle-nav` | references, definitions, callers, implementations, rename, impact |
| `shuttle-refactor` | AST-exact refactors (extract/inline/move/signature changes) |
| `shuttle-editcore` | 4-tier fuzzy search/replace application (hot path) |
| `shuttle-codemod` | ast-grep rule-driven, zero-token mechanical edits |
| `shuttle-conflict` | git merge conflict resolution engine |
| `shuttle-goal` | local Goal journal, CAS + fencing, artifacts, acceptance gates |
| `shuttle-lsp-bridge` | real LSP adapter, tier-3, feature-gated and off by default |
| `shuttle-runtime-tools` | runtime probes and tool availability detection |
| `shuttle-ui` | formal UI proposition evaluation |
| `shuttle-cli` | the `shuttle` binary — the host agent entry point |

## Before you open a pull request

Run the same gate CI runs, in this order:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release --locked
./target/release/shuttle --version
```

All four must pass. A green `cargo test` alone is not enough: the workspace lint
policy is part of the build, not decoration.

### Lint policy

The workspace shares one lint set (`[workspace.lints]` in the root `Cargo.toml`):

- `clippy::dbg_macro`, `clippy::todo`, `clippy::unimplemented` are **denied**.
  A debugging macro or a stub left in a commit fails the build by design.
- `unused_qualifications`, `unused_import_braces`, `unit_bindings` are warnings
  and the tree is currently clean under them; keep it that way.

Do not add per-crate `#![deny(...)]` preambles. If a lint should apply widely,
change the workspace policy and open a discussion about it.

## Tests

- Unit tests live inline in the module they cover.
- Binary-level integration tests live in `crates/shuttle-cli/tests/` and drive the
  real `shuttle` executable.
- Tests must be **hermetic**: create a `TempDir`, write minimal fixtures, and
  never depend on the developer's home directory, network access, or an
  installed ctags. If a test needs an external tool, skip it explicitly when the
  tool is absent rather than failing.
- Latency benches set `harness = false` and are std-only on purpose (no
  criterion dependency). Keep new benches consistent with that.

If you fix a bug, add the test that would have caught it. Red-then-green is the
expectation, not a preference.

## Changing a wire contract

`EditPlan`, `EditOp`, `ApplyOutcome`, and the `goal` bundle/run-plan envelopes
are consumed by host agents. When you change one:

1. Bump the contract version constant (for example `CURRENT_VERSION` in
   `shuttle-protocol`).
2. Keep the deserializer backward-tolerant only if the old shape is still
   unambiguous; otherwise reject explicitly instead of guessing.
3. Add a round-trip test and a rejection test for the old shape.
4. Add a `CHANGELOG.md` entry under `## [Unreleased]`.

Contract changes are user-visible; treat them as a minor version bump at minimum.

## Commits and pull requests

- Keep one coherent concern per commit. A refactor and a behaviour change in the
  same commit are hard to review and hard to revert.
- Write the commit message around *why* the change exists. "what" is in the diff.
- Reference the issue in the PR description and state how you verified the
  change. "Ran `cargo test --workspace`" is the minimum; a reproducer is better.
- Do not reformat unrelated files. Reviewers read the diff, not the whole tree.
- New dependencies need a short justification in the PR. shuttle stays
  dependency-light on purpose: the hot path must remain FFI-free and
  daemon-free.

## Changelog

This project follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Add your change
under `## [Unreleased]` in the appropriate section (`Added`, `Changed`,
`Fixed`, `Removed`, `Security`). The release process moves those entries into a
versioned section; do not edit released sections.

## License

By contributing, you agree that your contributions are licensed under the MIT
License (see [LICENSE](LICENSE)).

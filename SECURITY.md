# Security Policy

shuttle writes to source trees and can spawn bounded subprocesses on behalf of
an agent. A bug that lets an agent write outside its declared scope, escape a
`cell-run` boundary, or replay a stale controller operation is a security issue,
not just a correctness bug.

## Supported versions

| Version | Supported |
|---|---|
| `0.3.x` | yes |
| `0.2.x` and earlier | no |

Security fixes land on the latest `0.3.x` release.

## Reporting a vulnerability

**Preferred: GitHub private vulnerability reporting.** Open the repository's
`Security` tab and choose *Report a vulnerability*. The report stays private and
gives us a shared timeline.

**Backup: email** `<security-contact>`.
Replace this placeholder with a monitored mailbox before publishing the
repository; GitHub private reporting alone is sufficient if you prefer not to
publish an address.

Please do **not** open a public issue, pull request, or discussion for a
suspected vulnerability.

Include what you can:

- affected version (`shuttle --version`) and platform
- a minimal reproducer: the `EditPlan`/bundle JSON or CLI invocation
- the observed behaviour and the boundary you believe was crossed
- impact: read, write, execution, or denial of service
- any suggested fix or mitigation

We aim to acknowledge within 3 business days and to give a first assessment
within 10. There is no bug bounty programme and no formal SLA.

## Threat model

shuttle is a local execution kernel. Trust boundaries that matter:

| # | Surface | In scope |
|---|---|---|
| 1 | **Path scope enforcement** (`--enforce-scope`, `--allowed-path`, `cell-run` bindings) | Writes escaping the declared scope via `..`, absolute paths, symlinks, hard links, or TOCTOU races |
| 2 | **`cell-run` write boundary** | A `cell_binding/v1` envelope that fails open, or a run that writes the source tree when the binding forbids it |
| 3 | **Command execution** (`allowed_commands`) | Shell metacharacter injection; the design is exact-argv matching with no shell — any deviation is a bug |
| 4 | **Revision control** | CAS or fencing-epoch bypass leading to a lost update or a replayed controller operation |
| 5 | **Goal journal and artifacts** (`shuttle-goal`) | Journal tampering, artifact path traversal, digest mismatch accepted, unbounded artifact growth |
| 6 | **Receipt and log hygiene** | Secrets, tokens, or absolute host paths leaking into receipts, traces, or `--json` output |
| 7 | **Resource exhaustion** | Unbounded child output capture, unbounded concurrency in `batch`/`run`, a plan that pins the machine |
| 8 | **LSP bridge** (tier-3, feature-gated, off by default) | Malicious or compromised language server responses causing out-of-scope writes |

## Out of scope

- **Wrong-but-in-scope edits.** If the model proposes a bad change and shuttle
  applies it faithfully inside the allowed path, that is a quality issue, not a
  vulnerability. Judge model output with the evaluation harness instead.
- **Vulnerabilities in third-party tools** (`universal-ctags`, `ast-grep`,
  `tree-sitter`, language servers) or in the host agent. Report those upstream;
  tell us if shuttle's integration makes the impact worse.
- **Running on untrusted content without isolation.** shuttle is designed to
  modify the working tree you point it at. Treat an untrusted repository the way
  you would treat untrusted code: run it in a container or VM.
- **Denial of service from a deliberately enormous repository** unless it
  bypasses an existing documented bound.
- **Missing features** such as sandboxing by default, network egress filtering,
  or multi-tenant isolation. These are design discussions, not vulnerabilities.

## Safe defaults to rely on

- `apply` supports `--dry-run`; use it when integrating a new planner.
- `cell-run` is fail-closed: an incomplete or contradictory binding refuses to
  start rather than running with an inferred boundary.
- The local Goal backend is single-host and append-only by design; it claims no
  distributed consensus and no remote write authority.
- The LSP bridge is feature-gated and disabled by default; enabling it widens
  the trust boundary to the language server you point it at.

## Disclosure

We will coordinate disclosure with you. Once a fix is available we publish a
GitHub Security Advisory, credit the reporter unless you ask otherwise, and add
a `Security` entry to `CHANGELOG.md`. Please give us a reasonable window before
any public write-up.

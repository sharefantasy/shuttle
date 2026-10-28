//! `shuttle impact --contract` (G6 · S1): the cross-language contract-graph query.
//!
//! Given the *changed IDL files*, answer "changing this message/RPC touches
//! which implementation files, in which languages?" — walking only **provable**
//! [`shuttle_symbols::graph::EdgeKind::ContractDirected`] edges (IDL declared it and
//! ≥2 language sides codegen'd it). This is the daemonless realization of the
//! §2.3 design: every hit is a file that the IDL toolchain provably produced,
//! never a name coincidence.
//!
//! # §7 degrade ladder (the A4 cure, made machine-readable)
//! Each contract node the changed IDL declared is classified into exactly one
//! bucket, and the [`ContractImpact::degraded`] flag / [`ContractImpact::notes`]
//! surface every non-High outcome instead of laundering it into a green edge:
//!
//! | node state | outcome |
//! |---|---|
//! | codegen anchored on ≥2 languages | `ContractDirected` cross-language hits, High |
//! | codegen anchored on exactly 1 language | `one-sided` — no cross-language edge, degrade |
//! | declared/changed but no codegen footprint anywhere | `stale-contract` — degrade |
//! | changed file is a known-but-unparsed IDL family | `unknown-idl-dialect` — degrade |

use crate::{Backend, Confidence, NavHit, NavKind, QueryOptions};
use shuttle_symbols::contract::{ContractKind, ContractSet};
use shuttle_symbols::contract_anchor::{nodes_declared_in, ContractAnchoring};
use std::collections::{BTreeMap, BTreeSet};

/// One provable cross-language contract hit: an impl file bound (via codegen)
/// to a contract node declared by a changed IDL.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContractHit {
    /// The contract node id (`namespace.Service.Method` / `namespace.Message`).
    pub node_id: String,
    /// Whether the node is a service, method, or message.
    pub kind: String,
    /// The impl file provably generated from the contract.
    pub path: String,
    /// The impl file's language.
    pub lang: String,
    /// 1-based line where the contract name is referenced in the impl file.
    pub line: usize,
}

/// The full result of a `--contract` query: the provable cross-language hits
/// plus every §7 degrade the walk encountered, so the caller never mistakes an
/// absent edge for a proven "no impact".
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContractImpact {
    /// Provable cross-language hits (each edge is `ContractDirected`, High).
    pub hits: Vec<ContractHit>,
    /// Contract nodes whose only codegen was one language (no cross-lang edge).
    pub one_sided: Vec<String>,
    /// Contract nodes declared/changed with no codegen footprint (drift).
    pub stale: Vec<String>,
    /// Changed files whose IDL dialect we recognized but do not yet parse.
    pub unknown_dialects: Vec<String>,
    /// True when any degrade fired — the answer is not a clean cross-language
    /// proof and the caller must not treat it as one.
    pub degraded: bool,
    /// Human-readable degrade notes, one per non-High outcome.
    pub notes: Vec<String>,
}

impl ContractImpact {
    /// Lower the cross-language hits into the unified [`NavHit`] model so the
    /// CLI renders them exactly like `shuttle impact`. Each hit is a `Caller` (the
    /// impl side depends on the contract) tagged High — the ContractDirected
    /// edge is a first-class trusted claim. Degrades are carried separately in
    /// [`ContractImpact::notes`] (they have no file to point at as a hit).
    pub fn to_nav_hits(&self) -> Vec<NavHit> {
        let mut hits: Vec<NavHit> = self
            .hits
            .iter()
            .map(|h| NavHit {
                path: h.path.clone(),
                line: h.line,
                col: None,
                kind: NavKind::Caller,
                symbol: h.node_id.clone(),
                snippet: None,
                confidence: Confidence::High,
                source: Backend::Ctags,
            })
            .collect();
        hits.sort_by(|a, b| {
            a.symbol
                .cmp(&b.symbol)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.line.cmp(&b.line))
        });
        hits
    }
}

/// Compute the cross-language contract impact of the changed IDL `changed`
/// files over `opts.root`. Reads the git-tracked file universe once, parses
/// every IDL into contract nodes, anchors codegen from the non-IDL sources, and
/// classifies each node the changed IDL declared. Pure adjudication on top of
/// the reads; the honesty ladder above governs every outcome.
pub fn contract_impact(
    changed: &[String],
    opts: &QueryOptions,
) -> Result<ContractImpact, crate::NavError> {
    let wanted: BTreeSet<String> = changed
        .iter()
        .filter(|c| !c.is_empty())
        .map(|c| normalize(c))
        .collect();
    if wanted.is_empty() {
        return Ok(ContractImpact::default());
    }

    // One read of the tracked universe, split into IDL vs impl sources. The
    // contract set is built over ALL idl files (so a changed message that
    // another idl references still resolves); anchoring runs over impl sources.
    // Each tracked file as (root-relative key, openable path). The key matches
    // how the CLI names `--changed` files (git-diff root-relative), so the
    // classification below can compare `c.path` against `wanted` directly.
    let files = tracked_files(&opts.root);
    let mut idl_contents: BTreeMap<String, String> = BTreeMap::new();
    let mut impl_contents: BTreeMap<String, String> = BTreeMap::new();
    for (rel, abs) in files {
        let Ok(src) = std::fs::read_to_string(&abs) else {
            continue;
        };
        let key = normalize(&rel);
        if is_idl_path(&key) {
            idl_contents.insert(key, src);
        } else if shuttle_symbols::lang_for_path(&key).is_some() {
            impl_contents.insert(key, src);
        }
    }

    Ok(impact_from_maps(&wanted, &idl_contents, &impl_contents))
}

/// The pure, I/O-free adjudication core of [`contract_impact`]. Given the
/// normalized set of *changed* IDL paths (`wanted`), the full IDL source map,
/// and the full impl source map, classify every contract node the changed IDL
/// declared into the §7 honesty ladder. This is deliberately free of git and
/// the filesystem so it is exhaustively unit-testable in any sandbox (the
/// git-tracked read that feeds it lives in [`contract_impact`]); both share one
/// adjudication so there is no second, drifting classifier.
pub fn impact_from_maps(
    wanted: &BTreeSet<String>,
    idl_contents: &BTreeMap<String, String>,
    impl_contents: &BTreeMap<String, String>,
) -> ContractImpact {
    let mut out = ContractImpact::default();
    if wanted.is_empty() {
        return out;
    }

    let set = ContractSet::build(idl_contents);
    let anchoring = ContractAnchoring::build(&set, impl_contents);

    // Changed files that are a known-but-unparsed IDL family degrade up front.
    for f in wanted {
        if set.unknown_dialects.contains(f) {
            out.unknown_dialects.push(f.clone());
            out.degraded = true;
            out.notes.push(format!(
                "unknown-idl-dialect: {f} recognized as IDL but not parsed"
            ));
        }
    }
    out.unknown_dialects.sort();
    out.unknown_dialects.dedup();

    // Classify every contract node the *changed* IDL files declared.
    let declared = nodes_declared_in(&set, wanted);
    let mut seen_ids: BTreeSet<String> = BTreeSet::new();
    for c in declared {
        let node_id = c.node_id();
        if !seen_ids.insert(node_id.clone()) {
            continue;
        }
        match anchoring.anchor(&node_id) {
            Some(a) if a.is_cross_language() => {
                for f in &a.files {
                    out.hits.push(ContractHit {
                        node_id: node_id.clone(),
                        kind: kind_str(c.kind).to_string(),
                        path: f.path.clone(),
                        lang: f.lang.clone(),
                        line: f.line,
                    });
                }
            }
            Some(_) => {
                out.one_sided.push(node_id.clone());
                out.degraded = true;
                out.notes.push(format!(
                    "one-sided: {node_id} codegen on a single language — no cross-language edge"
                ));
            }
            None => {
                out.stale.push(node_id.clone());
                out.degraded = true;
                out.notes.push(format!(
                    "stale-contract: {node_id} declared/changed but no codegen footprint"
                ));
            }
        }
    }

    out.hits.sort_by(|a, b| {
        a.node_id
            .cmp(&b.node_id)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
    out.hits.dedup();
    out.one_sided.sort();
    out.one_sided.dedup();
    out.stale.sort();
    out.stale.dedup();
    out.notes.sort();
    out.notes.dedup();
    out
}

/// Human-readable node kind for the JSON/`ContractHit`.
fn kind_str(kind: ContractKind) -> &'static str {
    match kind {
        ContractKind::Service => "service",
        ContractKind::Method => "method",
        ContractKind::Message => "message",
    }
}

/// IDL path recognition (mirrors `shuttle_symbols::contract::dialect_of` + the
/// known-but-unparsed families, without exposing the private fn). This decides
/// only "should this file be read as IDL"; the parse/degrade split is the
/// contract set's job.
fn is_idl_path(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    const IDL_EXTS: [&str; 10] = [
        ".proto", ".thrift", ".graphql", ".gql", ".openapi", ".fbs", ".capnp", ".avdl", ".avsc",
        ".smithy",
    ];
    IDL_EXTS.iter().any(|e| l.ends_with(e)) || l.ends_with(".openapi.yaml") || l.ends_with(".ice")
}

/// Normalize a path: `\\` → `/`, strip a leading `./`.
fn normalize(p: &str) -> String {
    let p = p.replace('\\', "/");
    p.strip_prefix("./").unwrap_or(&p).to_string()
}

/// Git-tracked files under `root`, as `(root-relative key, openable path)`
/// pairs. Falls back to an empty list when git is unavailable — the caller then
/// reports no contract nodes (never guesses a file universe).
fn tracked_files(root: &str) -> Vec<(String, String)> {
    let binary = match shuttle_runtime_tools::resolve(shuttle_runtime_tools::ToolId::Git) {
        Ok(binary) => binary,
        Err(_) => return Vec::new(),
    };
    let out = match std::process::Command::new(binary)
        .args(["-C", root, "ls-files", "-z"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let raw = String::from_utf8_lossy(&out.stdout);
    let r = root.trim_end_matches('/');
    raw.split('\0')
        .filter(|s| !s.is_empty())
        .map(|rel| {
            let abs = if r.is_empty() || r == "." {
                rel.to_string()
            } else {
                format!("{r}/{rel}")
            };
            (rel.to_string(), abs)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    // -----------------------------------------------------------------------
    // Fixture sources shared by both the pure (in-memory) and e2e (git) tests.
    // -----------------------------------------------------------------------

    const USER_PROTO: &str = "\
package user.api;
message GetUserRequest { string id = 1; }
service UserService {
  rpc GetUser (GetUserRequest) returns (GetUserRequest);
}
";
    const GO_GEN: &str = "\
// Code generated by protoc-gen-go. DO NOT EDIT.
type GetUserRequest struct{}
type UserService interface{}
func GetUser(r *GetUserRequest) {}
";
    const TS_GEN: &str = "\
// @generated by protoc-gen-ts_proto
export interface GetUserRequest {}
export interface UserService {}
export function GetUser(r: GetUserRequest) {}
";
    const UTIL_PROTO: &str = "\
package util;
message Empty {}
service Util {
  rpc helper (Empty) returns (Empty);
}
";

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (normalize(k), (*v).to_string()))
            .collect()
    }
    fn want(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|p| normalize(p)).collect()
    }

    // =======================================================================
    // Pure adjudication tests — no git, no filesystem. These prove the whole
    // §7 honesty ladder and run in every sandbox (including ones that block
    // `.git/config` writes). `contract_impact` is a thin git-read shim over
    // exactly this `impact_from_maps`, so covering the core covers the tool.
    // =======================================================================

    #[test]
    fn pure_cross_language_hit_spans_go_and_ts() {
        let idl = map(&[("api/user.proto", USER_PROTO)]);
        let imp = map(&[("gen/user.pb.go", GO_GEN), ("web/client.ts", TS_GEN)]);
        let out = impact_from_maps(&want(&["api/user.proto"]), &idl, &imp);
        assert!(
            !out.degraded,
            "clean cross-language proof, notes={:?}",
            out.notes
        );
        let langs: BTreeSet<&str> = out
            .hits
            .iter()
            .filter(|h| h.node_id == "user.api.UserService.GetUser")
            .map(|h| h.lang.as_str())
            .collect();
        assert!(
            langs.contains("go") && langs.contains("typescript"),
            "langs={langs:?}"
        );
        // Every hit is a method/service/message kind — never a bare guess.
        assert!(out.hits.iter().all(|h| h.node_id.starts_with("user.api")));
    }

    #[test]
    fn pure_a4_same_name_no_codegen_yields_no_cross_language_edge() {
        // Same `helper` name on two languages, but NEITHER carries a codegen
        // banner. The A4 cure: a bare name must never connect two languages.
        let idl = map(&[("api/util.proto", UTIL_PROTO)]);
        let imp = map(&[
            ("go/util.go", "package util\nfunc helper() {}\n"),
            ("py/util.py", "def helper():\n    pass\n"),
        ]);
        let out = impact_from_maps(&want(&["api/util.proto"]), &idl, &imp);
        assert!(
            out.hits.is_empty(),
            "A4: must not connect on a name, hits={:?}",
            out.hits
        );
        assert!(out.degraded);
        assert!(out.stale.iter().any(|n| n == "util.Util.helper"));
    }

    #[test]
    fn pure_stale_contract_when_no_codegen_regenerated() {
        let idl = map(&[("api/user.proto", USER_PROTO)]);
        // A hand-written file, no banner: proves nothing about the contract.
        let imp = map(&[("app/handler.go", "package app\n// GetUser handled here\n")]);
        let out = impact_from_maps(&want(&["api/user.proto"]), &idl, &imp);
        assert!(out.hits.is_empty());
        assert!(out
            .stale
            .iter()
            .any(|n| n == "user.api.UserService.GetUser"));
        assert!(out.notes.iter().any(|n| n.starts_with("stale-contract")));
        assert!(out.degraded);
    }

    #[test]
    fn pure_one_sided_codegen_degrades() {
        let idl = map(&[("api/user.proto", USER_PROTO)]);
        let imp = map(&[("gen/user.pb.go", GO_GEN)]);
        let out = impact_from_maps(&want(&["api/user.proto"]), &idl, &imp);
        assert!(out.hits.is_empty(), "one side is not cross-language");
        assert!(out
            .one_sided
            .iter()
            .any(|n| n == "user.api.UserService.GetUser"));
        assert!(out.degraded);
    }

    #[test]
    fn pure_unknown_idl_dialect_is_recorded_not_guessed() {
        let idl = map(&[("api/schema.fbs", "table Foo { id:int; }\n")]);
        let out = impact_from_maps(&want(&["api/schema.fbs"]), &idl, &BTreeMap::new());
        assert!(out.degraded);
        assert!(out.unknown_dialects.iter().any(|f| f == "api/schema.fbs"));
        assert!(out.hits.is_empty());
    }

    #[test]
    fn pure_empty_changed_is_empty_result() {
        let idl = map(&[("api/user.proto", USER_PROTO)]);
        let out = impact_from_maps(&BTreeSet::new(), &idl, &BTreeMap::new());
        assert!(out.hits.is_empty() && !out.degraded);
    }

    #[test]
    fn pure_only_changed_idl_declares_nodes() {
        // Two IDLs in the universe; only the changed one contributes nodes.
        let idl = map(&[
            ("api/user.proto", USER_PROTO),
            ("api/util.proto", UTIL_PROTO),
        ]);
        let imp = map(&[("gen/user.pb.go", GO_GEN), ("web/client.ts", TS_GEN)]);
        // Change only util.proto — user.proto's cross-language codegen must NOT
        // surface, because util declared no anchored node.
        let out = impact_from_maps(&want(&["api/util.proto"]), &idl, &imp);
        assert!(
            out.hits.is_empty(),
            "util change must not surface user hits, {:?}",
            out.hits
        );
        assert!(out.stale.iter().any(|n| n == "util.Util.helper"));
    }

    #[test]
    fn to_nav_hits_are_high_confidence_callers_sorted() {
        let idl = map(&[("api/user.proto", USER_PROTO)]);
        let imp = map(&[("gen/user.pb.go", GO_GEN), ("web/client.ts", TS_GEN)]);
        let out = impact_from_maps(&want(&["api/user.proto"]), &idl, &imp);
        let navs = out.to_nav_hits();
        assert!(!navs.is_empty());
        assert!(navs
            .iter()
            .all(|n| matches!(n.confidence, Confidence::High)));
        assert!(navs.iter().all(|n| matches!(n.kind, NavKind::Caller)));
        assert!(navs.iter().all(|n| matches!(n.source, Backend::Ctags)));
        // Emitted in stable (symbol, path, line) order: every adjacent pair is
        // non-decreasing. (NavHit is not `PartialEq`, so check the key tuples.)
        let keys: Vec<(&str, &str, usize)> = navs
            .iter()
            .map(|n| (n.symbol.as_str(), n.path.as_str(), n.line))
            .collect();
        assert!(
            keys.windows(2).all(|w| w[0] <= w[1]),
            "unsorted nav hits: {keys:?}"
        );
    }

    // =======================================================================
    // End-to-end tests over a real on-disk git repo. These exercise the full
    // `contract_impact` shim (git ls-files → read → adjudicate). They are
    // guarded by `can_init_git()` because some sandboxes permit the `git`
    // binary but block writes to `.git/config`; there the pure tests above
    // still cover every adjudication path, and these run on any dev machine /
    // CI. The guard is the same house convention `reverify.rs` uses.
    // =======================================================================

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let base = std::env::var("SHUTTLE_TEST_TMP")
            .or_else(|_| std::env::var("TMPDIR"))
            .unwrap_or_else(|_| ".".into());
        let dir = Path::new(&base).join(format!(
            "shuttle_contract_impact_{name}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// True only when a git repo can actually be *created* here. A bare
    /// `git --version` is not enough: some sandboxes permit the binary but
    /// block writes to `.git/config`, in which case the e2e tests below skip
    /// (the pure tests above still prove every path).
    fn can_init_git() -> bool {
        let probe = scratch("initprobe");
        let ok = Command::new("git")
            .args(["init", "-q", "--template="])
            .current_dir(&probe)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
            && probe.join(".git").join("config").exists();
        let _ = std::fs::remove_dir_all(&probe);
        ok
    }

    fn git_init_add(dir: &Path) {
        let run = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
        };
        // Empty template so a user's global `~/.gittemplates` hooks are not
        // copied (some are non-executable / permission-restricted in CI).
        run(&["init", "-q", "--template="]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&["add", "-A"]);
        run(&["commit", "-qm", "seed"]);
    }

    #[test]
    fn e2e_cross_language_hit_spans_go_and_ts() {
        if !can_init_git() {
            return;
        }
        let dir = scratch("xlang");
        write(dir.as_path(), "api/user.proto", USER_PROTO);
        write(dir.as_path(), "gen/user.pb.go", GO_GEN);
        write(dir.as_path(), "web/client.ts", TS_GEN);
        git_init_add(dir.as_path());
        let opts = QueryOptions::new(dir.to_string_lossy().to_string());
        let imp = contract_impact(&["api/user.proto".to_string()], &opts).unwrap();
        assert!(
            !imp.degraded,
            "clean cross-language proof, notes={:?}",
            imp.notes
        );
        let langs: BTreeSet<&str> = imp
            .hits
            .iter()
            .filter(|h| h.node_id == "user.api.UserService.GetUser")
            .map(|h| h.lang.as_str())
            .collect();
        assert!(
            langs.contains("go") && langs.contains("typescript"),
            "langs={langs:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn e2e_a4_same_name_no_codegen_yields_no_cross_language_edge() {
        if !can_init_git() {
            return;
        }
        let dir = scratch("a4");
        write(dir.as_path(), "api/util.proto", UTIL_PROTO);
        write(
            dir.as_path(),
            "go/util.go",
            "package util\nfunc helper() {}\n",
        );
        write(dir.as_path(), "py/util.py", "def helper():\n    pass\n");
        git_init_add(dir.as_path());
        let opts = QueryOptions::new(dir.to_string_lossy().to_string());
        let imp = contract_impact(&["api/util.proto".to_string()], &opts).unwrap();
        assert!(
            imp.hits.is_empty(),
            "A4: must not connect two languages on a name, hits={:?}",
            imp.hits
        );
        assert!(imp.degraded);
        assert!(imp.stale.iter().any(|n| n == "util.Util.helper"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn e2e_stale_contract_when_no_codegen_regenerated() {
        if !can_init_git() {
            return;
        }
        let dir = scratch("stale");
        write(dir.as_path(), "api/user.proto", USER_PROTO);
        write(
            dir.as_path(),
            "app/handler.go",
            "package app\n// GetUser handled here\n",
        );
        git_init_add(dir.as_path());
        let opts = QueryOptions::new(dir.to_string_lossy().to_string());
        let imp = contract_impact(&["api/user.proto".to_string()], &opts).unwrap();
        assert!(imp.hits.is_empty());
        assert!(imp
            .stale
            .iter()
            .any(|n| n == "user.api.UserService.GetUser"));
        assert!(imp.notes.iter().any(|n| n.starts_with("stale-contract")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn e2e_one_sided_codegen_degrades() {
        if !can_init_git() {
            return;
        }
        let dir = scratch("oneside");
        write(dir.as_path(), "api/user.proto", USER_PROTO);
        write(dir.as_path(), "gen/user.pb.go", GO_GEN);
        git_init_add(dir.as_path());
        let opts = QueryOptions::new(dir.to_string_lossy().to_string());
        let imp = contract_impact(&["api/user.proto".to_string()], &opts).unwrap();
        assert!(imp.hits.is_empty(), "one side is not cross-language");
        assert!(imp
            .one_sided
            .iter()
            .any(|n| n == "user.api.UserService.GetUser"));
        assert!(imp.degraded);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn e2e_unknown_idl_dialect_is_recorded_not_guessed() {
        if !can_init_git() {
            return;
        }
        let dir = scratch("unknown");
        write(dir.as_path(), "api/schema.fbs", "table Foo { id:int; }\n");
        git_init_add(dir.as_path());
        let opts = QueryOptions::new(dir.to_string_lossy().to_string());
        let imp = contract_impact(&["api/schema.fbs".to_string()], &opts).unwrap();
        assert!(imp.degraded);
        assert!(imp.unknown_dialects.iter().any(|f| f == "api/schema.fbs"));
        assert!(imp.hits.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn e2e_empty_changed_is_empty_result() {
        if !can_init_git() {
            return;
        }
        let dir = scratch("empty");
        write(dir.as_path(), "api/user.proto", USER_PROTO);
        git_init_add(dir.as_path());
        let opts = QueryOptions::new(dir.to_string_lossy().to_string());
        let imp = contract_impact(&[], &opts).unwrap();
        assert!(imp.hits.is_empty() && !imp.degraded);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

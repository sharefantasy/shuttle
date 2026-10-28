//! Composite recipe (组合层, G2) — the shuttle-native, in-tree equivalent of
//! OpenRewrite's declarative `recipeList` . A recipe is a **named + versioned, ordered operator sequence**: it adds
//! *no* new transformation power (its `op`s are exactly the batchable operators)
//! — it only fixes a reusable sequence with bound parameters, then lowers into a
//! [`BatchSpec`] that runs through the exact same transactional gate as
//! `shuttle refactor batch` (Low-confidence reject / per-step compose / full reparse
//! / snapshot rollback).
//!
//! What this module deliberately is NOT (§3 / §5):
//! - no LST / type-attribution (that stays in the operator layer, pluggable);
//! - no fixed-point iteration, no scripting/conditions/loops in `bind`;
//! - no artifact distribution — just an in-tree registry with a lint.
//!
//! Boundaries baked into the format itself (§4.1):
//! 1. `op` ∈ the batchable whitelist ([`BATCHABLE_OPS`]) — recipes compose, never invent.
//! 2. `bind` values are literals or `{{param}}` templates only — no expressions.
//! 3. a required param with no default that is missing at call time → hard fail
//!    (§7「绝不瞎猜」, never guess a default).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The operator names a recipe step may reference. Exactly the source-driven
/// ops `shuttle refactor batch` can plan (see `plan_step` in the CLI). A recipe adds
/// no transformation power; to add a new transform, extend the operator layer
/// first, then reference it here.
pub const BATCHABLE_OPS: &[&str] = &[
    "remove-parameter",
    "inline-parameter",
    "introduce-parameter",
    "change-signature",
];

// ---------------------------------------------------------------------------
// BatchSpec / BatchStepSpec — the lowering target. Lives here (not in the CLI)
// so `expand` can *produce* it and the CLI can *consume* the same type, whether
// the spec was read from a JSON file (`batch`) or expanded from a recipe.
// ---------------------------------------------------------------------------

/// A transactional batch: an ordered list of source-based refactor steps.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BatchSpec {
    pub steps: Vec<BatchStepSpec>,
}

/// One batch step: an `op` plus its source-driven arguments. Only source-based
/// (no cursor line/col) refactors are batchable — the batch core threads the
/// *text* of earlier edits into later steps, so a cursor offset would be stale.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BatchStepSpec {
    /// Human label (optional; defaults to the op name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Which refactor to run, kebab-case (`remove-parameter`, `change-signature`, …).
    pub op: String,
    /// The file the refactor targets.
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub func: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr: Option<String>,
    /// `--params` mini-DSL string for change-signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<String>,
    #[serde(default = "String::new")]
    pub root: String,
}

// ---------------------------------------------------------------------------
// Recipe data model (§4.1)
// ---------------------------------------------------------------------------

/// A named + versioned, ordered operator sequence. Built-in recipes are static
/// (`&'static str` / `&'static [..]`), so this is `Serialize`-only (for
/// `--list --json`); it is never deserialized from untrusted input in G2.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Recipe {
    /// Dotted namespace + name, e.g. `perf.bitable-sync-pack` (§4.6-2).
    pub name: &'static str,
    /// Behaviour fingerprint (§4.6-3): bump when the step sequence/params change
    /// behaviour. One name has exactly one current version — not artifact-style
    /// multi-version coexistence.
    pub version: u32,
    pub description: &'static str,
    /// Recipe-level parameter declarations.
    pub params: &'static [RecipeParam],
    /// The ordered operator sequence (may reference recipe params via `{{name}}`).
    pub steps: &'static [RecipeStep],
}

/// A recipe-level input parameter.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RecipeParam {
    pub name: &'static str,
    pub required: bool,
    /// Only meaningful when `required == false` (§4.1-3).
    pub default: Option<&'static str>,
    pub doc: &'static str,
}

/// One operator invocation in a recipe. `bind` maps an operator-parameter name
/// (`func` / `param` / `expr` / `params` / `root`) to either a literal or a
/// string containing `{{param}}` template references.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RecipeStep {
    pub op: &'static str,
    pub label: Option<&'static str>,
    /// (operator-param-name, literal-or-`{{param}}`-template).
    pub bind: &'static [(&'static str, &'static str)],
}

/// Errors from validating / expanding a recipe (all honest hard failures — a
/// recipe never guesses, §7).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RecipeError {
    #[error("no such recipe `{0}` (run `shuttle refactor recipe --list`)")]
    NotFound(String),
    #[error("recipe `{name}` is version {have}, but --expect-version {want} was requested")]
    VersionMismatch { name: String, have: u32, want: u32 },
    #[error("recipe `{recipe}`: required param `{param}` was not supplied and has no default")]
    MissingRequiredParam { recipe: String, param: String },
    #[error("recipe `{recipe}`: param `{param}` is not declared (run --list to see its params)")]
    UnknownParam { recipe: String, param: String },
    #[error("recipe `{recipe}` step `{op}`: template references undeclared/unset param `{param}`")]
    UnresolvedTemplate {
        recipe: String,
        op: String,
        param: String,
    },
    #[error("recipe registry lint failed: {0}")]
    Lint(String),
    #[error("cannot read recipe file: {0}")]
    FileIo(String),
    #[error("recipe file is not valid JSON: {0}")]
    FileParse(String),
    #[error("recipe file rejected: {0}")]
    FileCap(String),
}

// ---------------------------------------------------------------------------
// Built-in registry (§4.6-1: single source of truth) + lint.
// ---------------------------------------------------------------------------

/// The built-in, read-only recipe table (§4.6-1: single source of truth for
/// shipped recipes). External, out-of-tree recipes are ALSO supported now via
/// [`RecipeFile`] (`*.recipe.json`) — unlocked once the in-tree abstraction
/// proved it generalizes (a second built-in recipe landed). The external path
/// reuses the *same* lint + expand core ([`lint_view`] / [`expand_view`]) as
/// this table, adding only runtime schema validation + untrusted-input caps; it
/// gains no transformation power.
pub static RECIPES: &[Recipe] = &[
    Recipe {
        name: "perf.promote-sync-tunables",
        version: 1,
        description: "把逐条同步函数里硬编码的攒批阈值 / 刷写水位线提成可调参数(两步 introduce-parameter,后一步观察到前一步的新签名)",
        params: &[
            RecipeParam {
                name: "func",
                required: true,
                default: None,
                doc: "目标同步函数名(逐条 sync 的那个函数)",
            },
            RecipeParam {
                name: "batch_size",
                required: false,
                default: Some("500"),
                doc: "函数体里当前硬编码的攒批阈值字面量(要提成参数的那个数)",
            },
            RecipeParam {
                name: "watermark",
                required: false,
                default: Some("100"),
                doc: "函数体里当前硬编码的刷写水位线字面量(要提成参数的那个数)",
            },
        ],
        steps: &[
            RecipeStep {
                op: "introduce-parameter",
                label: Some("攒批阈值提成参数 flush_at"),
                bind: &[
                    ("func", "{{func}}"),
                    ("param", "flush_at"),
                    ("expr", "{{batch_size}}"),
                ],
            },
            RecipeStep {
                op: "introduce-parameter",
                label: Some("刷写水位线提成参数 high_watermark"),
                bind: &[
                    ("func", "{{func}}"),
                    ("param", "high_watermark"),
                    ("expr", "{{watermark}}"),
                ],
            },
        ],
    },
    Recipe {
        name: "api.remove-unused-tail-resequence",
        version: 1,
        description: "删掉未使用的尾参数,再按目标 API 顺序重排剩余参数(remove-parameter + change-signature)",
        params: &[
            RecipeParam {
                name: "func",
                required: true,
                default: None,
                doc: "目标函数名",
            },
            RecipeParam {
                name: "removed_param",
                required: true,
                default: None,
                doc: "要删除的未使用尾参数名(必须已是尾参数)",
            },
            RecipeParam {
                name: "new_params",
                required: false,
                default: Some("keep:1,keep:0"),
                doc: "change-signature 的 --params DSL, 索引基于删掉尾参数后的剩余参数列表",
            },
        ],
        steps: &[
            RecipeStep {
                op: "remove-parameter",
                label: Some("删除未使用的尾参数"),
                bind: &[("func", "{{func}}"), ("param", "{{removed_param}}")],
            },
            RecipeStep {
                op: "change-signature",
                label: Some("按新 API 顺序重排剩余参数"),
                bind: &[("func", "{{func}}"), ("params", "{{new_params}}")],
            },
        ],
    },
];

// ---------------------------------------------------------------------------
// Borrowed view — the ONE shape lint + expand operate on, so a built-in
// `Recipe` (`&'static`) and an owned external `RecipeFile` (heap) share exactly
// one code path (§4.6: external recipes gain no power, only a new source).
// ---------------------------------------------------------------------------

/// A borrowed, lifetime-generic view of any recipe (built-in or file-loaded).
pub struct RecipeView<'a> {
    pub name: &'a str,
    pub version: u32,
    pub params: Vec<ParamView<'a>>,
    pub steps: Vec<StepView<'a>>,
}

/// Borrowed view of one declared parameter.
pub struct ParamView<'a> {
    pub name: &'a str,
    pub required: bool,
    pub default: Option<&'a str>,
}

/// Borrowed view of one step (`op` + its `(key, template)` binds).
pub struct StepView<'a> {
    pub op: &'a str,
    pub label: Option<&'a str>,
    pub bind: Vec<(&'a str, &'a str)>,
}

impl Recipe {
    /// Borrow this built-in recipe as the shared view.
    fn view(&self) -> RecipeView<'_> {
        RecipeView {
            name: self.name,
            version: self.version,
            params: self
                .params
                .iter()
                .map(|p| ParamView {
                    name: p.name,
                    required: p.required,
                    default: p.default,
                })
                .collect(),
            steps: self
                .steps
                .iter()
                .map(|st| StepView {
                    op: st.op,
                    label: st.label,
                    bind: st.bind.iter().map(|(k, v)| (*k, *v)).collect(),
                })
                .collect(),
        }
    }
}

/// Look up a recipe by name in the built-in registry.
pub fn find(name: &str) -> Option<&'static Recipe> {
    // Accept both the bare name (`perf.promote-sync-tunables`) and the
    // versioned form the run banner / docs / tools advertise (`name@1`). One
    // name has exactly one current version, so an explicit `@N` must match that
    // version or the lookup misses on purpose — a stale `@2` never resolves to
    // v1. A trailing `@` that is not a valid version is treated as part of the
    // (non-matching) name rather than silently ignored.
    let (bare, want_version) = match name.rsplit_once('@') {
        Some((b, v)) => match v.parse::<u32>() {
            Ok(n) => (b, Some(n)),
            Err(_) => (name, None),
        },
        None => (name, None),
    };
    RECIPES
        .iter()
        .find(|r| r.name == bare && want_version.is_none_or(|v| v == r.version))
}

/// Lint the whole built-in registry once (§4.6-1). A bad recipe is rejected
/// *here*, at the registry boundary, never dragged to run time:
/// - names are unique;
/// - every `step.op` is in the batchable whitelist;
/// - every `{{param}}` template references a declared param.
pub fn lint_registry() -> Result<(), RecipeError> {
    let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
    for r in RECIPES {
        if seen.insert(r.name, ()).is_some() {
            return Err(RecipeError::Lint(format!(
                "duplicate recipe name `{}`",
                r.name
            )));
        }
        lint_recipe(r)?;
    }
    Ok(())
}

/// Lint a single built-in recipe (see [`lint_registry`]). Delegates to the
/// shared [`lint_view`] core so the built-in and external paths lint identically.
pub fn lint_recipe(r: &Recipe) -> Result<(), RecipeError> {
    lint_view(&r.view())
}

/// The single lint core, over the borrowed [`RecipeView`]. Both the built-in
/// registry and external `*.recipe.json` funnel through here:
/// - every `step.op` is in the batchable whitelist;
/// - every `{{param}}` template references a declared param.
pub fn lint_view(r: &RecipeView<'_>) -> Result<(), RecipeError> {
    let declared: BTreeMap<&str, ()> = r.params.iter().map(|p| (p.name, ())).collect();
    for step in &r.steps {
        if !BATCHABLE_OPS.contains(&step.op) {
            return Err(RecipeError::Lint(format!(
                "recipe `{}`: step op `{}` is not batchable (allowed: {})",
                r.name,
                step.op,
                BATCHABLE_OPS.join(", ")
            )));
        }
        for (_k, v) in &step.bind {
            for token in template_tokens(v) {
                if !declared.contains_key(token.as_str()) {
                    return Err(RecipeError::Lint(format!(
                        "recipe `{}` step `{}`: template references undeclared param `{}`",
                        r.name, step.op, token
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Extract every `{{name}}` token from a bind template (order-preserving, may
/// repeat). Non-template text is ignored.
fn template_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            if let Some(end) = s[i + 2..].find("}}") {
                let name = s[i + 2..i + 2 + end].trim().to_string();
                if !name.is_empty() {
                    out.push(name);
                }
                i = i + 2 + end + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Substitute every `{{name}}` in `template` using `values`. Any referenced
/// name absent from `values` is an [`RecipeError::UnresolvedTemplate`] — we
/// never leave a hole or guess.
fn render_template(
    recipe: &str,
    op: &str,
    template: &str,
    values: &BTreeMap<String, String>,
) -> Result<String, RecipeError> {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            if let Some(end) = template[i + 2..].find("}}") {
                let name = template[i + 2..i + 2 + end].trim();
                let val = values
                    .get(name)
                    .ok_or_else(|| RecipeError::UnresolvedTemplate {
                        recipe: recipe.to_string(),
                        op: op.to_string(),
                        param: name.to_string(),
                    })?;
                out.push_str(val);
                i = i + 2 + end + 2;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// expand: recipe + params -> BatchSpec (pure; §4.2)
// ---------------------------------------------------------------------------

/// Resolve the recipe's declared params against caller-supplied `args`, then
/// expand every step's `bind` into a [`BatchStepSpec`] targeting `file`,
/// producing a [`BatchSpec`] byte-identical to a hand-written batch. This does
/// **no** I/O, gating or writing — the CLI hands the result straight to the
/// shared `execute_batch` core.
///
/// Rules (§4.1 / §5):
/// - a required param missing with no default → [`RecipeError::MissingRequiredParam`];
/// - an `arg` naming an undeclared param → [`RecipeError::UnknownParam`];
/// - a `{{param}}` with no resolved value → [`RecipeError::UnresolvedTemplate`].
pub fn expand(
    recipe: &Recipe,
    file: &str,
    args: &BTreeMap<String, String>,
) -> Result<BatchSpec, RecipeError> {
    expand_view(&recipe.view(), file, args)
}

/// The single expand core, over the borrowed [`RecipeView`]. Built-in `expand`
/// and external [`RecipeFile::expand`] both delegate here, so an out-of-tree
/// recipe lowers into a byte-identical [`BatchSpec`] under the exact same rules
/// (§4.1 / §5). Same honest hard-errors: missing required / unknown param /
/// unresolved template — never a guessed default (§7).
pub fn expand_view(
    recipe: &RecipeView<'_>,
    file: &str,
    args: &BTreeMap<String, String>,
) -> Result<BatchSpec, RecipeError> {
    // Reject unknown --set keys up front (never silently ignore).
    for k in args.keys() {
        if !recipe.params.iter().any(|p| p.name == k) {
            return Err(RecipeError::UnknownParam {
                recipe: recipe.name.to_string(),
                param: k.clone(),
            });
        }
    }

    // Resolve each declared param: supplied > default > (required → error).
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    for p in &recipe.params {
        if let Some(v) = args.get(p.name) {
            values.insert(p.name.to_string(), v.clone());
        } else if let Some(d) = p.default {
            values.insert(p.name.to_string(), d.to_string());
        } else if p.required {
            return Err(RecipeError::MissingRequiredParam {
                recipe: recipe.name.to_string(),
                param: p.name.to_string(),
            });
        }
        // optional + no default + not supplied → simply unset; a step that
        // templates it will fail loudly via render_template (never guessed).
    }

    let mut steps = Vec::with_capacity(recipe.steps.len());
    for step in &recipe.steps {
        let mut spec = BatchStepSpec {
            label: step.label.map(|l| l.to_string()),
            op: step.op.to_string(),
            file: file.to_string(),
            func: None,
            param: None,
            expr: None,
            params: None,
            root: String::new(),
        };
        for (key, tmpl) in &step.bind {
            let rendered = render_template(recipe.name, step.op, tmpl, &values)?;
            // (key, tmpl are &&str via &step.bind)
            match *key {
                "func" => spec.func = Some(rendered),
                "param" => spec.param = Some(rendered),
                "expr" => spec.expr = Some(rendered),
                "params" => spec.params = Some(rendered),
                "root" => spec.root = rendered,
                other => {
                    return Err(RecipeError::Lint(format!(
                        "recipe `{}` step `{}`: unknown bind key `{}` (allowed: func, param, expr, params, root)",
                        recipe.name, step.op, other
                    )))
                }
            }
        }
        steps.push(spec);
    }

    Ok(BatchSpec { steps })
}

// ---------------------------------------------------------------------------
// External `*.recipe.json` (§4.6): owned, deserialized-from-untrusted-input
// recipe. It reuses the SAME lint + expand core as the built-in registry
// ([`lint_view`] / [`expand_view`]) — so it gains no transformation power; it
// only adds (a) a JSON on-disk source and (b) untrusted-input caps enforced
// before the shared lint ever runs.
// ---------------------------------------------------------------------------

/// Hard caps on a file-loaded recipe, applied BEFORE lint/expand. These bound
/// the blast radius of untrusted input (§4.6 sandboxing); they are intentionally
/// generous but finite so a malformed/hostile file cannot balloon work.
pub const MAX_FILE_RECIPE_BYTES: usize = 64 * 1024;
pub const MAX_FILE_RECIPE_STEPS: usize = 64;
pub const MAX_FILE_RECIPE_PARAMS: usize = 64;
pub const MAX_FILE_RECIPE_BINDS_PER_STEP: usize = 16;

/// Owned mirror of [`RecipeParam`] for deserialization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecipeFileParam {
    pub name: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub doc: String,
}

/// Owned mirror of [`RecipeStep`] for deserialization. `bind` is an ordered map
/// of operator-param-name -> literal-or-`{{param}}` template.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecipeFileStep {
    pub op: String,
    #[serde(default)]
    pub label: Option<String>,
    /// Preserve author order (BatchSpec cares about none, but keeps output
    /// deterministic and mirrors the built-in tuple order).
    pub bind: BTreeMap<String, String>,
}

/// An out-of-tree composite recipe loaded from a `*.recipe.json`. Same shape as
/// a built-in [`Recipe`], but owned and untrusted — so it is capped, then
/// linted through the identical [`lint_view`], then expanded through the
/// identical [`expand_view`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecipeFile {
    pub name: String,
    #[serde(default = "default_file_version")]
    pub version: u32,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub params: Vec<RecipeFileParam>,
    pub steps: Vec<RecipeFileStep>,
}

fn default_file_version() -> u32 {
    1
}

impl RecipeFile {
    /// Parse + cap-check a recipe from JSON bytes (no disk I/O). Rejects
    /// oversized input, empty name, and over-cap step/param/bind counts BEFORE
    /// the shared lint runs, so caps bound the work regardless of contents.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, RecipeError> {
        if bytes.len() > MAX_FILE_RECIPE_BYTES {
            return Err(RecipeError::FileCap(format!(
                "recipe file is {} bytes, over the {}-byte cap",
                bytes.len(),
                MAX_FILE_RECIPE_BYTES
            )));
        }
        let rf: RecipeFile =
            serde_json::from_slice(bytes).map_err(|e| RecipeError::FileParse(e.to_string()))?;
        rf.check_caps()?;
        // Lint through the SAME core the built-in registry uses.
        lint_view(&rf.view())?;
        Ok(rf)
    }

    /// Read + parse + cap-check + lint a recipe from a `*.recipe.json` path.
    pub fn load(path: &str) -> Result<Self, RecipeError> {
        let bytes = std::fs::read(path).map_err(|e| RecipeError::FileIo(format!("{path}: {e}")))?;
        Self::from_json_bytes(&bytes)
    }

    /// Enforce the untrusted-input caps (called before lint/expand).
    fn check_caps(&self) -> Result<(), RecipeError> {
        if self.name.trim().is_empty() {
            return Err(RecipeError::FileCap("recipe name must not be empty".into()));
        }
        if self.steps.is_empty() {
            return Err(RecipeError::FileCap(format!(
                "recipe `{}` has no steps",
                self.name
            )));
        }
        if self.steps.len() > MAX_FILE_RECIPE_STEPS {
            return Err(RecipeError::FileCap(format!(
                "recipe `{}` has {} steps, over the {} cap",
                self.name,
                self.steps.len(),
                MAX_FILE_RECIPE_STEPS
            )));
        }
        if self.params.len() > MAX_FILE_RECIPE_PARAMS {
            return Err(RecipeError::FileCap(format!(
                "recipe `{}` declares {} params, over the {} cap",
                self.name,
                self.params.len(),
                MAX_FILE_RECIPE_PARAMS
            )));
        }
        for st in &self.steps {
            if st.bind.len() > MAX_FILE_RECIPE_BINDS_PER_STEP {
                return Err(RecipeError::FileCap(format!(
                    "recipe `{}` step `{}` has {} binds, over the {} cap",
                    self.name,
                    st.op,
                    st.bind.len(),
                    MAX_FILE_RECIPE_BINDS_PER_STEP
                )));
            }
        }
        Ok(())
    }

    /// Borrow as the shared [`RecipeView`], so lint + expand run the identical
    /// code path as a built-in recipe.
    pub fn view(&self) -> RecipeView<'_> {
        RecipeView {
            name: &self.name,
            version: self.version,
            params: self
                .params
                .iter()
                .map(|p| ParamView {
                    name: &p.name,
                    required: p.required,
                    default: p.default.as_deref(),
                })
                .collect(),
            steps: self
                .steps
                .iter()
                .map(|st| StepView {
                    op: &st.op,
                    label: st.label.as_deref(),
                    bind: st
                        .bind
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_str()))
                        .collect(),
                })
                .collect(),
        }
    }

    /// Expand into a [`BatchSpec`] via the shared [`expand_view`] core.
    pub fn expand(
        &self,
        file: &str,
        args: &BTreeMap<String, String>,
    ) -> Result<BatchSpec, RecipeError> {
        expand_view(&self.view(), file, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn builtin_registry_passes_lint() {
        lint_registry().expect("built-in recipes must lint clean");
    }

    #[test]
    fn find_hits_and_misses() {
        assert!(find("perf.promote-sync-tunables").is_some());
        assert!(find("api.remove-unused-tail-resequence").is_some());
        assert!(find("nope.not-real").is_none());
    }

    #[test]
    fn find_accepts_versioned_suffix() {
        // The run banner and docs advertise `name@1`; that form must resolve.
        let bare = find("perf.promote-sync-tunables").unwrap();
        let versioned = find("perf.promote-sync-tunables@1").unwrap();
        assert_eq!(bare.name, versioned.name);
        assert_eq!(versioned.version, 1);
        // A mismatched explicit version misses on purpose (no multi-version
        // coexistence — `@2` must not silently fall back to v1).
        assert!(find("perf.promote-sync-tunables@2").is_none());
        // A non-numeric `@suffix` is not a version → treated as part of the
        // (non-existent) name, so it misses rather than matching the bare name.
        assert!(find("perf.promote-sync-tunables@latest").is_none());
    }

    #[test]
    fn expand_substitutes_params_and_defaults() {
        let r = find("perf.promote-sync-tunables").unwrap();
        let spec = expand(r, "sync.py", &args(&[("func", "push_rows")])).unwrap();
        assert_eq!(spec.steps.len(), 2);
        // step 1: introduce-parameter flush_at, expr = default batch_size (500).
        assert_eq!(spec.steps[0].op, "introduce-parameter");
        assert_eq!(spec.steps[0].file, "sync.py");
        assert_eq!(spec.steps[0].func.as_deref(), Some("push_rows"));
        assert_eq!(spec.steps[0].param.as_deref(), Some("flush_at"));
        assert_eq!(spec.steps[0].expr.as_deref(), Some("500"));
        // step 2: introduce-parameter high_watermark, expr = default watermark (100).
        assert_eq!(spec.steps[1].op, "introduce-parameter");
        assert_eq!(spec.steps[1].func.as_deref(), Some("push_rows"));
        assert_eq!(spec.steps[1].param.as_deref(), Some("high_watermark"));
        assert_eq!(spec.steps[1].expr.as_deref(), Some("100"));
    }

    #[test]
    fn expand_respects_supplied_optional_param() {
        let r = find("perf.promote-sync-tunables").unwrap();
        let spec = expand(
            r,
            "s.py",
            &args(&[("func", "f"), ("batch_size", "1000"), ("watermark", "250")]),
        )
        .unwrap();
        assert_eq!(spec.steps[0].expr.as_deref(), Some("1000"));
        assert_eq!(spec.steps[1].expr.as_deref(), Some("250"));
    }

    #[test]
    fn expand_second_recipe_uses_default_resequence() {
        let r = find("api.remove-unused-tail-resequence").unwrap();
        let spec = expand(
            r,
            "api.py",
            &args(&[("func", "normalize_pair"), ("removed_param", "unused")]),
        )
        .unwrap();
        assert_eq!(spec.steps.len(), 2);
        assert_eq!(spec.steps[0].op, "remove-parameter");
        assert_eq!(spec.steps[0].file, "api.py");
        assert_eq!(spec.steps[0].func.as_deref(), Some("normalize_pair"));
        assert_eq!(spec.steps[0].param.as_deref(), Some("unused"));
        assert_eq!(spec.steps[1].op, "change-signature");
        assert_eq!(spec.steps[1].func.as_deref(), Some("normalize_pair"));
        assert_eq!(spec.steps[1].params.as_deref(), Some("keep:1,keep:0"));
    }

    #[test]
    fn expand_second_recipe_respects_supplied_resequence() {
        let r = find("api.remove-unused-tail-resequence").unwrap();
        let spec = expand(
            r,
            "api.py",
            &args(&[
                ("func", "normalize_pair"),
                ("removed_param", "unused"),
                ("new_params", "keep:0,keep:1"),
            ]),
        )
        .unwrap();
        assert_eq!(spec.steps[1].params.as_deref(), Some("keep:0,keep:1"));
    }

    #[test]
    fn missing_required_param_is_hard_error() {
        let r = find("perf.promote-sync-tunables").unwrap();
        let err = expand(r, "s.py", &args(&[])).unwrap_err();
        assert_eq!(
            err,
            RecipeError::MissingRequiredParam {
                recipe: "perf.promote-sync-tunables".into(),
                param: "func".into()
            }
        );
    }

    #[test]
    fn unknown_set_key_is_rejected() {
        let r = find("perf.promote-sync-tunables").unwrap();
        let err = expand(r, "s.py", &args(&[("func", "f"), ("bogus", "1")])).unwrap_err();
        assert_eq!(
            err,
            RecipeError::UnknownParam {
                recipe: "perf.promote-sync-tunables".into(),
                param: "bogus".into()
            }
        );
    }

    #[test]
    fn template_tokens_extraction() {
        assert_eq!(template_tokens("{{a}}"), vec!["a".to_string()]);
        assert_eq!(
            template_tokens("keep:0,adddef:flush_at={{batch_size}}"),
            vec!["batch_size".to_string()]
        );
        assert!(template_tokens("no templates here").is_empty());
        assert_eq!(
            template_tokens("{{x}}-{{y}}"),
            vec!["x".to_string(), "y".to_string()]
        );
    }

    #[test]
    fn render_rejects_unresolved_template() {
        let vals = args(&[]);
        let err = render_template("r", "op", "{{ghost}}", &vals).unwrap_err();
        assert_eq!(
            err,
            RecipeError::UnresolvedTemplate {
                recipe: "r".into(),
                op: "op".into(),
                param: "ghost".into()
            }
        );
    }

    #[test]
    fn lint_catches_non_batchable_op() {
        let bad = Recipe {
            name: "bad.op",
            version: 1,
            description: "",
            params: &[],
            steps: &[RecipeStep {
                op: "inline-variable",
                label: None,
                bind: &[],
            }],
        };
        assert!(matches!(lint_recipe(&bad), Err(RecipeError::Lint(_))));
    }

    #[test]
    fn lint_catches_undeclared_template_param() {
        let bad = Recipe {
            name: "bad.tmpl",
            version: 1,
            description: "",
            params: &[],
            steps: &[RecipeStep {
                op: "remove-parameter",
                label: None,
                bind: &[("func", "{{missing}}")],
            }],
        };
        assert!(matches!(lint_recipe(&bad), Err(RecipeError::Lint(_))));
    }

    // --- External *.recipe.json (RecipeFile) tests -------------------------

    const FILE_JSON: &str = r#"{
        "name": "custom.tidy-tail",
        "version": 1,
        "description": "external: drop unused tail param then resequence",
        "params": [
            { "name": "func", "required": true },
            { "name": "removed_param", "required": true },
            { "name": "new_params", "required": false, "default": "keep:1,keep:0" }
        ],
        "steps": [
            { "op": "remove-parameter", "label": "drop tail", "bind": { "func": "{{func}}", "param": "{{removed_param}}" } },
            { "op": "change-signature", "bind": { "func": "{{func}}", "params": "{{new_params}}" } }
        ]
    }"#;

    #[test]
    fn recipe_file_parses_and_expands_like_builtin() {
        let rf = RecipeFile::from_json_bytes(FILE_JSON.as_bytes()).unwrap();
        assert_eq!(rf.name, "custom.tidy-tail");
        assert_eq!(rf.version, 1);
        let spec = rf
            .expand(
                "api.py",
                &args(&[("func", "f"), ("removed_param", "unused")]),
            )
            .unwrap();
        assert_eq!(spec.steps.len(), 2);
        assert_eq!(spec.steps[0].op, "remove-parameter");
        assert_eq!(spec.steps[0].file, "api.py");
        assert_eq!(spec.steps[0].func.as_deref(), Some("f"));
        assert_eq!(spec.steps[0].param.as_deref(), Some("unused"));
        assert_eq!(spec.steps[1].op, "change-signature");
        // default resequence applied.
        assert_eq!(spec.steps[1].params.as_deref(), Some("keep:1,keep:0"));
    }

    #[test]
    fn recipe_file_shares_expand_rules_missing_required() {
        // Same honest hard-error as the built-in path: required param absent.
        let rf = RecipeFile::from_json_bytes(FILE_JSON.as_bytes()).unwrap();
        let err = rf.expand("api.py", &args(&[("func", "f")])).unwrap_err();
        assert_eq!(
            err,
            RecipeError::MissingRequiredParam {
                recipe: "custom.tidy-tail".into(),
                param: "removed_param".into()
            }
        );
    }

    #[test]
    fn recipe_file_shares_expand_rules_unknown_set_key() {
        let rf = RecipeFile::from_json_bytes(FILE_JSON.as_bytes()).unwrap();
        let err = rf
            .expand(
                "api.py",
                &args(&[("func", "f"), ("removed_param", "u"), ("bogus", "1")]),
            )
            .unwrap_err();
        assert_eq!(
            err,
            RecipeError::UnknownParam {
                recipe: "custom.tidy-tail".into(),
                param: "bogus".into()
            }
        );
    }

    #[test]
    fn recipe_file_lint_rejects_non_batchable_op() {
        // The SAME lint core rejects an off-whitelist op at load time.
        let bad = r#"{
            "name": "custom.bad",
            "steps": [ { "op": "inline-variable", "bind": {} } ]
        }"#;
        let err = RecipeFile::from_json_bytes(bad.as_bytes()).unwrap_err();
        assert!(matches!(err, RecipeError::Lint(_)), "got {err:?}");
    }

    #[test]
    fn recipe_file_lint_rejects_undeclared_template_param() {
        let bad = r#"{
            "name": "custom.ghost",
            "steps": [ { "op": "remove-parameter", "bind": { "func": "{{nope}}" } } ]
        }"#;
        let err = RecipeFile::from_json_bytes(bad.as_bytes()).unwrap_err();
        assert!(matches!(err, RecipeError::Lint(_)), "got {err:?}");
    }

    #[test]
    fn recipe_file_rejects_bad_json() {
        let err = RecipeFile::from_json_bytes(b"{ not json").unwrap_err();
        assert!(matches!(err, RecipeError::FileParse(_)), "got {err:?}");
    }

    #[test]
    fn recipe_file_cap_rejects_oversize() {
        let big = vec![b'x'; MAX_FILE_RECIPE_BYTES + 1];
        let err = RecipeFile::from_json_bytes(&big).unwrap_err();
        assert!(matches!(err, RecipeError::FileCap(_)), "got {err:?}");
    }

    #[test]
    fn recipe_file_cap_rejects_empty_name_and_no_steps() {
        let no_steps = r#"{ "name": "x", "steps": [] }"#;
        assert!(matches!(
            RecipeFile::from_json_bytes(no_steps.as_bytes()).unwrap_err(),
            RecipeError::FileCap(_)
        ));
        let empty_name =
            r#"{ "name": "  ", "steps": [ { "op": "remove-parameter", "bind": {} } ] }"#;
        assert!(matches!(
            RecipeFile::from_json_bytes(empty_name.as_bytes()).unwrap_err(),
            RecipeError::FileCap(_)
        ));
    }

    #[test]
    fn recipe_file_cap_rejects_too_many_steps() {
        let mut steps = String::new();
        for i in 0..(MAX_FILE_RECIPE_STEPS + 1) {
            if i > 0 {
                steps.push(',');
            }
            steps.push_str(r#"{ "op": "remove-parameter", "bind": {} }"#);
        }
        let json = format!(r#"{{ "name": "custom.many", "steps": [{steps}] }}"#);
        let err = RecipeFile::from_json_bytes(json.as_bytes()).unwrap_err();
        assert!(matches!(err, RecipeError::FileCap(_)), "got {err:?}");
    }

    #[test]
    fn recipe_file_version_defaults_to_one() {
        let json =
            r#"{ "name": "custom.v", "steps": [ { "op": "remove-parameter", "bind": {} } ] }"#;
        let rf = RecipeFile::from_json_bytes(json.as_bytes()).unwrap();
        assert_eq!(rf.version, 1);
    }
}

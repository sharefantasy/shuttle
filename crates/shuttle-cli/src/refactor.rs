//! `shuttle refactor` subcommand dispatcher.
//!
//! Maps `shuttle refactor <op>` to the corresponding `shuttle_refactor::*` function and,
//! by default, prints a dry-run preview (per-site diff + confidence). `--write`
//! lowers the [`shuttle_refactor::RefactorPlan`]'s `EditPlan` and applies it through
//! the same all-or-nothing gate as `shuttle apply --gate` / `shuttle rename` (probe then
//! commit). Low-confidence plans block the write unless `--force` (§9.5 gate).

use std::collections::BTreeMap;

use shuttle_refactor::{
    apply_batch, change_signature, create_from_usage, extract_constant,
    extract_method_with_symbols, extract_to_new_file, extract_variable, generate_boilerplate,
    implement_members, inline_constant, inline_method, inline_parameter, inline_variable,
    introduce_parameter,
    locals::Lang,
    move_function, new_module_wiring,
    query::{select, QueryCapture},
    recipe::{self, Recipe, RecipeError},
    record_projection, remove_parameter, rename_local, rename_symbol, scaffold_test, BatchSpec,
    BatchStep, BatchStepSpec, NewParam, RefactorPlan, Vfs,
};

#[derive(Debug, clap::Subcommand)]
pub enum RefactorCmd {
    /// Rename the local variable governing a cursor, function-scoped (§4).
    RenameLocal {
        /// File to refactor (language inferred from extension: py/js).
        file: String,
        /// 1-based line of the cursor.
        #[arg(long)]
        line: usize,
        /// 0-based byte column of the cursor.
        #[arg(long)]
        col: usize,
        /// New identifier.
        #[arg(long = "to")]
        new_name: String,
        /// Apply to disk (default is dry-run preview).
        #[arg(long)]
        write: bool,
        /// Allow writing even when the plan is Low-confidence.
        #[arg(long)]
        force: bool,
        /// Emit JSON (RefactorPlan) instead of the human diff.
        #[arg(long)]
        json: bool,
    },
    /// Rename a top-level / global symbol and every reference across the repo
    /// (§7: cross-file textual discovery via `shuttle references`, capped at Medium).
    RenameSymbol {
        /// File the rename is invoked from (defines same-file vs cross-file).
        file: String,
        /// Current symbol name.
        #[arg(long = "name")]
        name: String,
        /// New identifier.
        #[arg(long = "to")]
        new_name: String,
        /// Repo root for cross-file reference discovery (`shuttle references`). Omit
        /// to limit the rename to the origin file only.
        #[arg(long, default_value = "")]
        root: String,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Inline a single-assignment local into its references, delete the def (§4).
    InlineVariable {
        file: String,
        #[arg(long)]
        line: usize,
        #[arg(long)]
        col: usize,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Hoist a selected expression into a fresh local declaration (§4).
    ExtractVariable {
        file: String,
        #[arg(long)]
        line: usize,
        /// 0-based byte column where the selected expression starts.
        #[arg(long = "start-col")]
        start_col: usize,
        /// 0-based byte column where the selected expression ends (exclusive).
        #[arg(long = "end-col")]
        end_col: usize,
        #[arg(long = "name")]
        new_name: String,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Promote a selected literal into a module-level named constant and
    /// rewrite every structurally-identical literal in the file (§4).
    ExtractConstant {
        file: String,
        #[arg(long)]
        line: usize,
        /// 0-based byte column where the selected literal starts.
        #[arg(long = "start-col")]
        start_col: usize,
        /// 0-based byte column where the selected literal ends (exclusive).
        #[arg(long = "end-col")]
        end_col: usize,
        #[arg(long = "name")]
        const_name: String,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Inline a single-definition module constant: substitute its value at every
    /// reference and delete the declaration (inverse of extract-constant).
    InlineConstant {
        file: String,
        #[arg(long)]
        line: usize,
        #[arg(long)]
        col: usize,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Extract a line range into a new function; derive params/returns via
    /// locals + run the §3.4 legality gate (py/js/go/rust/ts).
    ExtractMethod {
        file: String,
        /// 1-based first line of the selection (inclusive).
        #[arg(long = "start-line")]
        start_line: usize,
        /// 1-based last line of the selection (inclusive).
        #[arg(long = "end-line")]
        end_line: usize,
        #[arg(long = "name")]
        new_name: String,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Inline the callee of the call at a cursor into the call site; expand its
    /// body / result expression and bind arguments to parameters (§4). Same-file
    /// only; multi-return / side-effect cases downgrade or reject.
    InlineMethod {
        file: String,
        /// 1-based line of the cursor on the call.
        #[arg(long)]
        line: usize,
        /// 0-based byte column of the cursor on the call.
        #[arg(long)]
        col: usize,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Change a same-file function's parameter list (reorder / delete / add) and
    /// synchronise call sites: same-file structurally, cross-file via `shuttle
    /// callers` (§4). Second-tier: unsafe sites downgrade or are skipped.
    ChangeSignature {
        file: String,
        /// Name of the function whose signature changes.
        #[arg(long)]
        func: String,
        /// Target parameter list as comma-separated entries, in order. Each is
        /// `keep:<old_index>` (0-based), `add:<decl>=<default_arg>` (injects the
        /// value at every call site, e.g. `add:count: i32=0`), or
        /// `adddef:<decl>` (a *defaulted* trailing add whose default lives in the
        /// signature, e.g. `adddef:count=0`, so existing calls omit it — Python/JS/TS
        /// only). Example: `--params 'keep:1,keep:0,adddef:flag=false'`.
        #[arg(long)]
        params: String,
        /// Repo root for cross-file call-site discovery (`shuttle callers`). Omit to
        /// limit the rewrite to this file only.
        #[arg(long, default_value = "")]
        root: String,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Promote a constant expression inside a function body into a new trailing
    /// parameter, passing the original expression at every call site (same file
    /// structurally, cross-file via `shuttle callers`). Rejected if the expression
    /// reads a function-internal binding (§7).
    IntroduceParameter {
        file: String,
        /// Name of the function whose body contains the expression.
        #[arg(long)]
        func: String,
        /// The exact expression text to promote (e.g. `10`, `"utf-8"`).
        #[arg(long)]
        expr: String,
        /// Name of the new parameter.
        #[arg(long = "name")]
        param_name: String,
        /// Repo root for cross-file call-site discovery (`shuttle callers`). Omit to
        /// limit the rewrite to this file only.
        #[arg(long, default_value = "")]
        root: String,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Inline a trailing parameter whose value is the same trivial constant at
    /// every call site: substitute that constant into the body where the
    /// parameter is read, drop it from the signature, and drop the trailing
    /// argument at each call. Rejects (§7) if call sites disagree, an argument
    /// is non-trivial, an arity mismatches, or the parameter is not trailing.
    InlineParameter {
        file: String,
        /// Name of the function whose trailing parameter is being inlined.
        #[arg(long)]
        func: String,
        /// Name of the trailing parameter to inline.
        #[arg(long = "name")]
        param_name: String,
        /// Repo root for cross-file call-site discovery (`shuttle callers`). Omit to
        /// limit the rewrite to this file only.
        #[arg(long, default_value = "")]
        root: String,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Remove a trailing parameter that is never read in the function body,
    /// dropping the trailing argument at every call site. Unlike inline-parameter
    /// no value agreement is required (the parameter is unused). Rejects (§7) if
    /// the parameter is read, not trailing, defaulted, or an arity mismatches.
    RemoveParameter {
        file: String,
        /// Name of the function whose trailing parameter is being removed.
        #[arg(long)]
        func: String,
        /// Name of the trailing parameter to remove.
        #[arg(long = "name")]
        param_name: String,
        /// Repo root for cross-file call-site discovery (`shuttle callers`). Omit to
        /// limit the rewrite to this file only.
        #[arg(long, default_value = "")]
        root: String,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Move a uniquely-defined top-level function from one file to another:
    /// delete it at the origin, append it at the destination (created if
    /// absent). Imports/callers are reported but NOT rewritten (§7). Always
    /// Medium-confidence.
    MoveFunction {
        /// Source file the function currently lives in.
        file: String,
        /// Name of the top-level function to move.
        #[arg(long)]
        func: String,
        /// Destination file (created if it does not exist).
        #[arg(long = "to")]
        dest: String,
        /// Repo root for cross-file caller discovery (`shuttle callers`). Omit to
        /// skip the blast-radius report.
        #[arg(long, default_value = "")]
        root: String,
        /// Opt-in: prepend a best-effort origin-side import so this file's
        /// remaining references keep resolving. JS/TS derive a precise relative
        /// path; Python is best-effort (flagged); Go same-dir is a no-op; Go
        /// cross-package and Rust degrade with an honest warning (§7).
        #[arg(long = "add-import")]
        add_import: bool,
        /// Opt-in: rewrite cross-file JS/TS caller imports to the destination
        /// where `func` is the sole named import (needs --root). Other shapes
        /// (co-import / namespace / non-JS-TS) are left untouched with a
        /// warning (§7). No effect without --root.
        #[arg(long = "rewrite-callers")]
        rewrite_callers: bool,
        /// Opt-in: delete a now-stale `import { func } from '<origin>'` in the
        /// destination file (a self-import once `func` is defined there). JS/TS
        /// sole-named-import only; other shapes degrade with a warning (§7).
        #[arg(long = "clean-dest-import")]
        clean_dest_import: bool,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Extract a uniquely-defined top-level item (a function OR a
    /// type/container — class, struct, enum, trait, interface, type alias) out
    /// of its file and into a **brand-new** file (§9 E4). Always creates the
    /// destination; refuses if it already exists (that is `move-function`'s
    /// job). The item is relocated verbatim; imports/callers are reported, not
    /// rewritten, so the plan is Medium (§7).
    ExtractToNewFile {
        /// Source file the item currently lives in.
        file: String,
        /// Name of the top-level function or type/container to extract.
        #[arg(long)]
        name: String,
        /// Destination file to create (must NOT already exist).
        #[arg(long = "to")]
        dest: String,
        /// Repo root for cross-file caller discovery (`shuttle callers`). Omit to
        /// skip the blast-radius report.
        #[arg(long, default_value = "")]
        root: String,
        /// Opt-in: prepend a best-effort origin-side import so this file's
        /// remaining references keep resolving (same machinery as
        /// `move-function`; §7 degrade rules apply).
        #[arg(long = "add-import")]
        add_import: bool,
        /// Opt-in: re-point cross-file JS/TS caller imports at the new file
        /// where `name` is the sole named import (needs --root). Other shapes
        /// degrade with a warning (§7).
        #[arg(long = "rewrite-callers")]
        rewrite_callers: bool,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Generate stub implementations for the members of a contract — a Rust
    /// trait, Go interface, TS interface, or Python ABC (§9 E2). Each
    /// unimplemented member gets a placeholder body (`todo!()` / `panic` /
    /// `throw new Error` / `raise NotImplementedError`); members that already
    /// carry a default body are skipped. The scaffold is inserted right after
    /// the contract's definition, verbatim-anchored, so nothing is guessed
    /// (§7). High confidence — a pure AST projection.
    ImplementMembers {
        /// File containing the contract (and where the impl is written).
        file: String,
        /// Name of the trait / interface / ABC to implement.
        #[arg(long)]
        interface: String,
        /// Concrete type name the stubs are generated for.
        #[arg(long = "as")]
        target: String,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Generate field-derived boilerplate for a data type (§9 E3): a
    /// constructor (all langs), Rust `Default`, or Python `__repr__`/`__eq__`.
    /// Every line is a mechanical projection of the type's declared fields —
    /// nothing is invented (§7) — so the plan is High confidence.
    GenerateBoilerplate {
        /// File containing the type (and where the boilerplate is written).
        file: String,
        /// Name of the struct / class / type to project fields from.
        #[arg(long = "type")]
        type_name: String,
        /// What to generate: `constructor` (all), `default` (Rust),
        /// `repr` / `eq` (Python).
        #[arg(long, default_value = "constructor")]
        what: String,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Stub an *undefined* function from one of its call sites (§9 E1): arity
    /// and parameter names come from the arguments, literal-argument types are
    /// inferred, and everything unknowable from a call (non-literal arg types,
    /// the return type, the body) is a called-out placeholder. Medium
    /// confidence — a call site under-determines the definition (§7).
    CreateFromUsage {
        /// File containing the call site (and where the stub is written).
        file: String,
        /// Name of the undefined function to stub from its usage.
        #[arg(long)]
        name: String,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Scaffold a table-driven test skeleton for a defined function (§9 E5):
    /// arity, parameter names, and parameter/return types are projected from
    /// the signature; the case table and the assertion are left as TODO
    /// placeholders. Medium confidence — the body logic is intentionally
    /// unwritten (§7). Rust `#[cfg(test)] mod`, Go table test, TS/JS Jest
    /// `it.each`, Python `pytest.mark.parametrize`.
    ScaffoldTest {
        /// File containing the function (and where the test is written).
        file: String,
        /// Name of the function to scaffold a test for.
        #[arg(long)]
        name: String,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Create a new module file and wire it into its package aggregator (§9
    /// E6): `pub mod` (Rust `lib.rs`/`mod.rs`), `export * from` (TS/JS
    /// `index.ts`), or `from . import` (Python `__init__.py`). The wiring line
    /// is a mechanical projection of the module's file stem and its path
    /// relative to the aggregator (§7) — High confidence. Go same-dir needs no
    /// wiring and cross-dir import paths are not inferable, so both are
    /// rejected with an explanation.
    NewModuleWiring {
        /// Path of the new module file (created if absent).
        module: String,
        /// The package aggregator to wire it into (lib.rs / mod.rs /
        /// index.ts / __init__.py), also where the wiring line is written.
        #[arg(long)]
        aggregator: String,
        /// Optional symbol to re-export by name (else the whole module).
        #[arg(long)]
        symbol: Option<String>,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Generate a field-projection "record" builder for a data type (§9 E7):
    /// projects every declared field into a stable
    /// `build_record`-style payload (`{"kind": "<t>_record_v1", "fields": {..}}`)
    /// the sync tooling hand-writes for Bitable sync. Labels default to the
    /// field names verbatim (§7 — we don't guess the display titles). High
    /// confidence. Python module fn / TS method / Rust+Go sibling; JS rejected.
    RecordProjection {
        /// File containing the type (and where the builder is written).
        file: String,
        /// Name of the type/struct/class to project into a record.
        #[arg(long)]
        name: String,
        #[arg(long)]
        write: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Transactional batch refactor (§8): run an ordered list of source-based
    /// refactors from a JSON spec, all-or-nothing. Later steps observe the
    /// edits of earlier ones (composed in memory); the whole chain is gated on
    /// confidence + a composed re-parse before anything reaches disk. `--write`
    /// commits under a snapshot that is rolled back on any mid-write IO error.
    Batch {
        /// Path to the JSON batch spec (see docs: `{ "steps": [ { "op": ... } ] }`).
        spec: String,
        /// Repo root (reserved for future cross-file steps).
        #[arg(long, default_value = ".")]
        root: String,
        #[arg(long)]
        write: bool,
        /// Allow committing even if a step is Low-confidence.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Lower a refactor plan (from any op's `--json`) into an LSP `codeAction`
    /// with a `WorkspaceEdit` (§9.9 D2). Reads the RefactorPlan JSON from stdin
    /// and resolves each content-anchored op to a concrete text range against
    /// the on-disk files it names. Prints the codeAction JSON. Content-anchored
    /// ops that don't occur exactly once are refused (§7: never guess a range).
    LspActions {
        /// Files the plan edits (`path` values inside the plan). Their current
        /// on-disk text is used to resolve `search` strings to LSP ranges.
        #[arg(long = "file", num_args = 1..)]
        files: Vec<String>,
    },
    /// Read-only tree-sitter query probe (§G3): run an arbitrary `.scm` query
    /// against a file and print the captured nodes (capture name + range +
    /// line/col + kind + text) as JSON. Selects nodes only — never writes. The
    /// safe node-selection primitive operators build on; a malformed query is a
    /// hard error (§7: we never guess what the caller meant).
    QuerySelect {
        /// File to parse (its extension picks the language).
        file: String,
        /// The tree-sitter query (S-expression, `.scm` syntax).
        #[arg(long)]
        query: String,
    },
    /// Composite recipe (§G2): run a named + versioned, ordered operator
    /// sequence from the built-in registry. A recipe adds no new transformation
    /// power — it expands (with bound params) into a `BatchSpec` and runs
    /// through the exact same transactional gate as `shuttle refactor batch`
    /// (Low-confidence reject / per-step compose / full reparse / rollback).
    Recipe {
        /// Recipe name (e.g. `perf.bitable-sync-pack`). Omit with `--list` or
        /// `--from-file` (the file supplies the name).
        #[arg(default_value = "")]
        name: String,
        /// List built-in recipes (name / version / description / params) instead
        /// of executing one.
        #[arg(long)]
        list: bool,
        /// Load an out-of-tree recipe from a `*.recipe.json` instead of the
        /// built-in registry. Untrusted input: capped + linted through the same
        /// core as built-in recipes before it can run. Mutually exclusive with a
        /// positional `name` and with `--list`.
        #[arg(long, value_name = "PATH")]
        from_file: Option<String>,
        /// The file the recipe's operators target.
        #[arg(long)]
        file: Option<String>,
        /// Bind a recipe parameter: `--set key=value` (repeatable).
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Refuse to run unless the resolved recipe is exactly this version
        /// (guards against "I thought I was running v1's playbook").
        #[arg(long)]
        expect_version: Option<u32>,
        #[arg(long)]
        write: bool,
        /// Allow committing even if a step is Low-confidence.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cmd: RefactorCmd) -> anyhow::Result<()> {
    match cmd {
        RefactorCmd::RenameLocal {
            file,
            line,
            col,
            new_name,
            write,
            force,
            json,
        } => {
            let source = std::fs::read_to_string(&file)?;
            let plan = rename_local(&file, &source, line, col, &new_name)?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::RenameSymbol {
            file,
            name,
            new_name,
            root,
            write,
            force,
            json,
        } => {
            let plan = rename_symbol(&file, &name, &new_name, &root)?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::InlineVariable {
            file,
            line,
            col,
            write,
            force,
            json,
        } => {
            let source = std::fs::read_to_string(&file)?;
            let plan = inline_variable(&file, &source, line, col, ())?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::ExtractVariable {
            file,
            line,
            start_col,
            end_col,
            new_name,
            write,
            force,
            json,
        } => {
            let source = std::fs::read_to_string(&file)?;
            let plan = extract_variable(&file, &source, line, start_col, end_col, &new_name)?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::ExtractConstant {
            file,
            line,
            start_col,
            end_col,
            const_name,
            write,
            force,
            json,
        } => {
            let source = std::fs::read_to_string(&file)?;
            let plan = extract_constant(&file, &source, line, start_col, end_col, &const_name)?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::InlineConstant {
            file,
            line,
            col,
            write,
            force,
            json,
        } => {
            let source = std::fs::read_to_string(&file)?;
            let plan = inline_constant(&file, &source, line, col, ())?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::ExtractMethod {
            file,
            start_line,
            end_line,
            new_name,
            write,
            force,
            json,
        } => {
            let source = std::fs::read_to_string(&file)?;
            // Level-2 type fallback (docs §5): typed languages (go/rust/ts) get a
            // repo ctags table so callee return types can fill placeholders. Best
            // effort — a missing ctags backend simply keeps the placeholders.
            let ext = std::path::Path::new(&file)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let symbols = if matches!(ext, "go" | "rs" | "ts" | "tsx" | "mts" | "cts") {
                shuttle_symbols::SymbolTable::scan(".").ok()
            } else {
                None
            };
            let plan = extract_method_with_symbols(
                &file,
                &source,
                start_line,
                end_line,
                &new_name,
                symbols.as_ref(),
            )?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::InlineMethod {
            file,
            line,
            col,
            write,
            force,
            json,
        } => {
            let source = std::fs::read_to_string(&file)?;
            let plan = inline_method(&file, &source, line, col)?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::ChangeSignature {
            file,
            func,
            params,
            root,
            write,
            force,
            json,
        } => {
            let source = std::fs::read_to_string(&file)?;
            let new_params = parse_params(&params)?;
            let plan = change_signature(&file, &source, &func, &new_params, &root)?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::IntroduceParameter {
            file,
            func,
            expr,
            param_name,
            root,
            write,
            force,
            json,
        } => {
            let source = std::fs::read_to_string(&file)?;
            let plan = introduce_parameter(&file, &source, &func, &expr, &param_name, &root)?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::InlineParameter {
            file,
            func,
            param_name,
            root,
            write,
            force,
            json,
        } => {
            let source = std::fs::read_to_string(&file)?;
            let plan = inline_parameter(&file, &source, &func, &param_name, &root)?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::RemoveParameter {
            file,
            func,
            param_name,
            root,
            write,
            force,
            json,
        } => {
            let source = std::fs::read_to_string(&file)?;
            let plan = remove_parameter(&file, &source, &func, &param_name, &root)?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::MoveFunction {
            file,
            func,
            dest,
            root,
            add_import,
            rewrite_callers,
            clean_dest_import,
            write,
            force,
            json,
        } => {
            let source = std::fs::read_to_string(&file)?;
            let plan = move_function(
                &file,
                &source,
                &func,
                &dest,
                &root,
                add_import,
                rewrite_callers,
                clean_dest_import,
            )?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::ExtractToNewFile {
            file,
            name,
            dest,
            root,
            add_import,
            rewrite_callers,
            write,
            force,
            json,
        } => {
            let source = std::fs::read_to_string(&file)?;
            let plan = extract_to_new_file(
                &file,
                &source,
                &name,
                &dest,
                &root,
                add_import,
                rewrite_callers,
            )?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::ImplementMembers {
            file,
            interface,
            target,
            write,
            force,
            json,
        } => {
            let source = std::fs::read_to_string(&file)?;
            let plan = implement_members(&file, &source, &interface, &target)?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::GenerateBoilerplate {
            file,
            type_name,
            what,
            write,
            force,
            json,
        } => {
            let source = std::fs::read_to_string(&file)?;
            let plan = generate_boilerplate(&file, &source, &type_name, &what)?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::CreateFromUsage {
            file,
            name,
            write,
            force,
            json,
        } => {
            let source = std::fs::read_to_string(&file)?;
            let plan = create_from_usage(&file, &source, &name)?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::ScaffoldTest {
            file,
            name,
            write,
            force,
            json,
        } => {
            let source = std::fs::read_to_string(&file)?;
            let plan = scaffold_test(&file, &source, &name)?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::NewModuleWiring {
            module,
            aggregator,
            symbol,
            write,
            force,
            json,
        } => {
            let agg_source = std::fs::read_to_string(&aggregator)?;
            let plan = new_module_wiring(&module, &aggregator, &agg_source, symbol.as_deref())?;
            finish(&aggregator, plan, write, force, json)
        }
        RefactorCmd::RecordProjection {
            file,
            name,
            write,
            force,
            json,
        } => {
            let source = std::fs::read_to_string(&file)?;
            let plan = record_projection(&file, &source, &name)?;
            finish(&file, plan, write, force, json)
        }
        RefactorCmd::Batch {
            spec,
            root,
            write,
            force,
            json,
        } => run_batch(&spec, &root, write, force, json),
        RefactorCmd::QuerySelect { file, query } => run_query_select(&file, &query),
        RefactorCmd::LspActions { files } => run_lsp_actions(&files),
        RefactorCmd::Recipe {
            name,
            list,
            from_file,
            file,
            set,
            expect_version,
            write,
            force,
            json,
        } => run_recipe(
            &name,
            list,
            from_file.as_deref(),
            file.as_deref(),
            &set,
            expect_version,
            write,
            force,
            json,
        ),
    }
}

/// §G3 probe: parse `file`, run `query`, print captures as JSON. Read-only.
fn run_query_select(file: &str, query: &str) -> anyhow::Result<()> {
    let lang = Lang::from_path(file)
        .ok_or_else(|| anyhow::anyhow!("unsupported language for path: {file}"))?;
    let source = std::fs::read_to_string(file)?;
    let caps = select(lang, &source, query)?;
    let json: Vec<serde_json::Value> = caps
        .iter()
        .map(|c: &QueryCapture| {
            serde_json::json!({
                "capture": c.capture,
                "start_byte": c.start_byte,
                "end_byte": c.end_byte,
                "line": c.line,
                "col": c.col,
                "kind": c.kind,
                "text": c.text,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

/// Parse the `--params` mini-DSL into an ordered `Vec<NewParam>`.
/// Entries are comma-separated; each is `keep:<idx>`, `add:<decl>=<default>`
/// (injected), or `adddef:<decl>` (defaulted, omitted at call sites).
fn parse_params(spec: &str) -> anyhow::Result<Vec<NewParam>> {
    let mut out = Vec::new();
    for raw in spec.split(',') {
        let entry = raw.trim();
        if entry.is_empty() {
            continue;
        }
        if let Some(rest) = entry.strip_prefix("keep:") {
            let idx: usize = rest
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid keep index in `--params`: `{entry}`"))?;
            out.push(NewParam::keep(idx));
        } else if let Some(rest) = entry.strip_prefix("adddef:") {
            let decl = rest.trim();
            if decl.is_empty() {
                anyhow::bail!("`adddef:` entry needs a declaration carrying a default: `{entry}`");
            }
            out.push(NewParam::add_defaulted(decl));
        } else if let Some(rest) = entry.strip_prefix("add:") {
            let (decl, default_arg) = rest.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("`add:` entry needs `<decl>=<default_arg>`: `{entry}`")
            })?;
            out.push(NewParam::add(decl.trim(), default_arg.trim()));
        } else {
            anyhow::bail!("unknown `--params` entry `{entry}` (expected `keep:<i>`, `add:<decl>=<default>`, or `adddef:<decl>`)");
        }
    }
    if out.is_empty() {
        anyhow::bail!("`--params` produced no parameters");
    }
    Ok(out)
}

/// Shared tail: dry-run preview, or `--write` apply through the editcore gate.
fn finish(
    file: &str,
    plan: RefactorPlan,
    write: bool,
    force: bool,
    json: bool,
) -> anyhow::Result<()> {
    if !write {
        if json {
            println!("{}", serde_json::to_string_pretty(&plan)?);
        } else {
            print_refactor_preview(&plan);
        }
        return Ok(());
    }

    // §9.5 gate: refuse to write a Low-confidence plan without --force.
    if plan.has_low_confidence() && !force {
        for w in &plan.warnings {
            eprintln!("warning: {w}");
        }
        anyhow::bail!(
            "refactor is Low-confidence. Re-run with --write --force to apply anyway, \
             or inspect with --json."
        );
    }
    if plan.plan.ops.is_empty() {
        eprintln!("no edits produced for {file}");
        return Ok(());
    }

    // Apply via the shared editcore path, all-or-nothing (probe then commit).
    let root = ".";
    let probe = crate::apply_pass(&plan.plan.ops, None, root, false)?;
    let all_ok = probe
        .iter()
        .all(|o| matches!(o.status, shuttle_protocol::ApplyStatus::Applied));
    let outcomes = if all_ok {
        crate::apply_pass(&plan.plan.ops, None, root, true)?
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
            eprintln!("{:?}: {n} edit(s) applied to {file}", plan.op);
        } else {
            anyhow::bail!("refactor aborted: some edits did not resolve (all-or-nothing)");
        }
    }
    Ok(())
}

/// Human-readable preview: intent + confidence + per-site line diffs.
fn print_refactor_preview(plan: &RefactorPlan) {
    let tag = match plan.confidence {
        shuttle_nav::Confidence::High => "HIGH",
        shuttle_nav::Confidence::Medium => "MED ",
        shuttle_nav::Confidence::Low => "LOW ",
    };
    if let Some(intent) = &plan.plan.intent {
        println!("[{tag}] {intent}");
    } else {
        println!("[{tag}] {:?}", plan.op);
    }
    for w in &plan.warnings {
        println!("  ! {w}");
    }
    for op in &plan.plan.ops {
        if let shuttle_protocol::EditOp::SearchReplace {
            search,
            replace,
            line_hint,
            ..
        } = op
        {
            let ln = line_hint.unwrap_or(0);
            for l in search.lines() {
                println!("  {ln}- {}", l.trim_end());
            }
            for l in replace.lines() {
                println!("  {ln}+ {}", l.trim_end());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Transactional batch (§8)
// ---------------------------------------------------------------------------

/// The JSON batch spec: an ordered list of source-based refactor steps. Each
/// step names an `op` plus its arguments; unknown fields are ignored. Only
/// source-based (no cursor line/col) refactors are batchable, because the batch
/// core threads the *text* of earlier edits into later steps — a cursor offset
/// would be invalidated the moment an earlier step shifts the file.
/// Plan one batch step against the supplied (composed) `source`, dispatching on
/// the kebab-case op name. Only source-driven refactors are supported here.
fn plan_step(step: &BatchStepSpec, source: &str) -> anyhow::Result<RefactorPlan> {
    let need = |o: &Option<String>, what: &str| -> anyhow::Result<String> {
        o.clone()
            .ok_or_else(|| anyhow::anyhow!("`{}` step needs `{what}`", step.op))
    };
    let plan = match step.op.as_str() {
        "remove-parameter" => remove_parameter(
            &step.file,
            source,
            &need(&step.func, "func")?,
            &need(&step.param, "param")?,
            &step.root,
        )?,
        "inline-parameter" => inline_parameter(
            &step.file,
            source,
            &need(&step.func, "func")?,
            &need(&step.param, "param")?,
            &step.root,
        )?,
        "introduce-parameter" => introduce_parameter(
            &step.file,
            source,
            &need(&step.func, "func")?,
            &need(&step.expr, "expr")?,
            &need(&step.param, "param")?,
            &step.root,
        )?,
        "change-signature" => {
            let params = step
                .params
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("`change-signature` step needs `params`"))?;
            let new_params = parse_params(params)?;
            change_signature(
                &step.file,
                source,
                &need(&step.func, "func")?,
                &new_params,
                &step.root,
            )?
        }
        other => anyhow::bail!(
            "op `{other}` is not batchable (batch supports source-driven ops: \
             remove-parameter, inline-parameter, introduce-parameter, change-signature)"
        ),
    };
    Ok(plan)
}

/// Read a JSON batch spec from disk and run it through the transactional core.
fn run_batch(
    spec_path: &str,
    _root: &str,
    write: bool,
    force: bool,
    json: bool,
) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(spec_path)
        .map_err(|e| anyhow::anyhow!("cannot read batch spec `{spec_path}`: {e}"))?;
    let spec: BatchSpec =
        serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("invalid batch spec JSON: {e}"))?;
    execute_batch(spec, write, force, json)
}

/// §G2: run (or `--list`) a built-in composite recipe. The recipe is looked up
/// in the in-tree registry, its params resolved from `--set key=value`, then
/// expanded into a `BatchSpec` and handed straight to the shared
/// `execute_batch` core — the recipe layer never touches write/gate/reparse.
#[allow(clippy::too_many_arguments)]
fn run_recipe(
    name: &str,
    list: bool,
    from_file: Option<&str>,
    file: Option<&str>,
    set: &[String],
    expect_version: Option<u32>,
    write: bool,
    force: bool,
    json: bool,
) -> anyhow::Result<()> {
    // The whole built-in registry is linted once, up front (§4.6-1): a bad
    // recipe is caught here, never dragged into an execution.
    recipe::lint_registry().map_err(|e| anyhow::anyhow!("{e}"))?;

    if list {
        if from_file.is_some() {
            anyhow::bail!(
                "`--list` lists the built-in registry; it cannot be combined with `--from-file`"
            );
        }
        return list_recipes(json);
    }

    // `--from-file` and a positional built-in `name` are mutually exclusive: a
    // file supplies its own name, so accepting both would be ambiguous.
    if from_file.is_some() && !name.is_empty() {
        anyhow::bail!("pass EITHER a built-in recipe name OR `--from-file <path>`, not both");
    }

    let file = file.ok_or_else(|| {
        anyhow::anyhow!(
            "`shuttle refactor recipe` needs `--file <f>` (the file its operators target)"
        )
    })?;

    // Parse `--set key=value` into the arg map (reject malformed / duplicate keys).
    let mut args: BTreeMap<String, String> = BTreeMap::new();
    for kv in set {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("`--set` expects `key=value`, got `{kv}`"))?;
        let (k, v) = (k.trim().to_string(), v.trim().to_string());
        if args.insert(k.clone(), v).is_some() {
            anyhow::bail!("`--set {k}=...` given more than once");
        }
    }

    // ---- External `*.recipe.json` path (§4.6): load + cap + lint through the
    // exact same core as a built-in recipe, then expand into a BatchSpec that
    // runs the identical transactional gate. It gains no transformation power.
    if let Some(path) = from_file {
        let rf = recipe::RecipeFile::load(path).map_err(recipe_err)?;
        if let Some(want) = expect_version {
            if rf.version != want {
                anyhow::bail!(
                    "recipe `{}` is version {}, but --expect-version {want} was requested",
                    rf.name,
                    rf.version
                );
            }
        }
        let spec: BatchSpec = rf.expand(file, &args).map_err(recipe_err)?;
        if !json {
            eprintln!(
                "recipe {}@{} (from {path}) — {}",
                rf.name, rf.version, rf.description
            );
        }
        return execute_batch(spec, write, force, json);
    }

    // ---- Built-in registry path.
    if name.is_empty() {
        anyhow::bail!("recipe name required (or pass --list, or --from-file <path>). e.g. `shuttle refactor recipe perf.bitable-sync-pack --file sync.py --set func=push_rows`");
    }
    let r: &Recipe = recipe::find(name).ok_or_else(|| {
        anyhow::anyhow!("no such recipe `{name}` (run `shuttle refactor recipe --list`)")
    })?;

    if let Some(want) = expect_version {
        if r.version != want {
            anyhow::bail!(
                "recipe `{name}` is version {}, but --expect-version {want} was requested",
                r.version
            );
        }
    }

    // Pure lower: recipe + args -> BatchSpec. Any missing required / unknown
    // param / unresolved template is an honest hard error here (§7 never guess).
    let spec: BatchSpec = recipe::expand(r, file, &args).map_err(recipe_err)?;

    if !json {
        eprintln!("recipe {}@{} — {}", r.name, r.version, r.description);
    }
    execute_batch(spec, write, force, json)
}

/// Turn a `RecipeError` into an anyhow error (kept separate so the message is
/// the recipe's own, honest wording).
fn recipe_err(e: RecipeError) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// `--list`: print the built-in registry as a human table or machine JSON.
fn list_recipes(json: bool) -> anyhow::Result<()> {
    if json {
        let arr: Vec<serde_json::Value> = recipe::RECIPES
            .iter()
            .map(|r| {
                let params: Vec<serde_json::Value> = r
                    .params
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "name": p.name,
                            "required": p.required,
                            "default": p.default,
                            "doc": p.doc,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "name": r.name,
                    "version": r.version,
                    "description": r.description,
                    "params": params,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    for r in recipe::RECIPES {
        println!("{}@{}  {}", r.name, r.version, r.description);
        for p in r.params {
            let req = if p.required { "required" } else { "optional" };
            let def = p
                .default
                .map(|d| format!(" (default: {d})"))
                .unwrap_or_default();
            println!("    - {} [{}]{}: {}", p.name, req, def, p.doc);
        }
    }
    Ok(())
}

/// The transactional core shared by `shuttle refactor batch` (spec read from disk)
/// and `shuttle refactor recipe` (spec expanded from a declarative recipe): plan
/// every step against the composed in-memory text, gate + reparse the whole
/// chain via `apply_batch`, then (with `--write`) commit under a snapshot that
/// is rolled back on any mid-write IO error.
fn execute_batch(spec: BatchSpec, write: bool, force: bool, json: bool) -> anyhow::Result<()> {
    if spec.steps.is_empty() {
        anyhow::bail!("batch spec has no steps");
    }

    // Seed the VFS with each distinct file's on-disk content, then thread the
    // composed text through so step N is planned against steps 1..N's edits.
    let mut vfs: Vfs = BTreeMap::new();
    let mut composed: Vfs = BTreeMap::new();
    let mut steps: Vec<BatchStep> = Vec::with_capacity(spec.steps.len());

    for step in &spec.steps {
        // Load the file into both maps on first touch.
        if !vfs.contains_key(&step.file) {
            let disk = std::fs::read_to_string(&step.file)
                .map_err(|e| anyhow::anyhow!("cannot read `{}`: {e}", step.file))?;
            vfs.insert(step.file.clone(), disk.clone());
            composed.insert(step.file.clone(), disk);
        }
        let source = composed.get(&step.file).unwrap().clone();
        let plan = plan_step(step, &source)?;
        // Advance the composed view so the *next* step sees this step's edits.
        if let Err((path, status)) = shuttle_refactor::compose_ops(&mut composed, &plan.plan.ops) {
            anyhow::bail!(
                "step `{}` cannot compose against prior edits ({path}: {status:?})",
                step.op
            );
        }
        let label = step.label.clone().unwrap_or_else(|| step.op.clone());
        steps.push(BatchStep::new(label, plan));
    }

    // Atomic gate + composed reparse over the *original* seed VFS.
    let outcome = match apply_batch(&vfs, &steps, force) {
        Ok(o) => o,
        Err(e) => anyhow::bail!("batch refused: {e}"),
    };

    if json {
        let report: Vec<_> = outcome
            .changed
            .keys()
            .map(|p| serde_json::json!({ "path": p }))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "steps": outcome.steps,
                "changed": report,
            }))?
        );
    } else {
        for line in &outcome.steps {
            eprintln!("{line}");
        }
        eprintln!("batch OK — {} file(s) changed", outcome.changed.len());
        for path in outcome.changed.keys() {
            eprintln!("  * {path}");
        }
    }

    if !write {
        eprintln!("(dry-run — re-run with --write to commit)");
        return Ok(());
    }

    // Snapshot every file we are about to overwrite, then write. On any IO
    // error mid-write, restore every snapshot so disk is left as we found it.
    let snapshot: BTreeMap<String, Option<String>> = outcome
        .changed
        .keys()
        .map(|p| (p.clone(), std::fs::read_to_string(p).ok()))
        .collect();

    let mut written: Vec<String> = Vec::new();
    let mut write_err: Option<anyhow::Error> = None;
    for (path, content) in &outcome.changed {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(path, content) {
            Ok(()) => written.push(path.clone()),
            Err(e) => {
                write_err = Some(anyhow::anyhow!("write `{path}` failed: {e}"));
                break;
            }
        }
    }

    if let Some(err) = write_err {
        for path in &written {
            match snapshot.get(path) {
                Some(Some(orig)) => {
                    let _ = std::fs::write(path, orig);
                }
                Some(None) => {
                    let _ = std::fs::remove_file(path); // was freshly created
                }
                None => {}
            }
        }
        anyhow::bail!(
            "batch write rolled back ({} file(s) restored): {err}",
            written.len()
        );
    }

    eprintln!("batch committed — {} file(s) written", written.len());
    Ok(())
}

/// Lower a `RefactorPlan` (JSON on stdin) into an LSP `codeAction`. Reads each
/// `--file`'s current text to resolve content-anchored ops to ranges.
fn run_lsp_actions(files: &[String]) -> anyhow::Result<()> {
    use std::io::Read;
    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .map_err(|e| anyhow::anyhow!("reading plan JSON from stdin: {e}"))?;
    let plan: RefactorPlan = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("invalid RefactorPlan JSON on stdin: {e}"))?;

    // Load every named file's current text into the source map the lowering
    // needs to turn `search` anchors into concrete ranges.
    let mut contents: BTreeMap<String, String> = BTreeMap::new();
    for f in files {
        let text =
            std::fs::read_to_string(f).map_err(|e| anyhow::anyhow!("cannot read `{f}`: {e}"))?;
        contents.insert(f.clone(), text);
    }
    let sources: shuttle_lsp_bridge::code_action::SourceMap = contents
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str()))
        .collect();

    match shuttle_lsp_bridge::code_action::code_action(&plan, &sources) {
        Some(action) => {
            println!("{}", shuttle_lsp_bridge::code_action::to_json(&action)?);
            Ok(())
        }
        None => anyhow::bail!(
            "plan could not be lowered to a codeAction — an op's anchor was \
             missing, ambiguous, or opaque (codemod). Ensure every `--file` the \
             plan touches is supplied and its text matches the plan's anchors."
        ),
    }
}

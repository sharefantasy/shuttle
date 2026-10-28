//! IDL contract layer (G6 · S1): the *semantic-truth* source above the
//! mechanical symbol graph. See .
//!
//! The symbol graph stops at the mechanical layer (who imports whom, who
//! references whose name). This module climbs one rung to the **contract /
//! type** layer: it treats an IDL (proto / thrift / openapi / graphql) as a
//! first-class truth source and parses it into `service` / `method` / `message`
//! nodes. Those nodes are what let a `Go server` and a `TS client` be linked by
//! a *provable* cross-language edge — "the IDL declared this RPC and both sides
//! generated it" — instead of the A4 病根 of guessing that two `helper`s are
//! the same symbol.
//!
//! # Guarantees / non-guarantees (§7 — never guess)
//! * Parsing is a one-shot **pure function** over file contents — no external
//!   IDL compiler, no daemon, no network.
//! * We only mint a contract node for a declaration we can *see* structurally
//!   (a `message`/`struct`/`service`/`rpc` header). An ambiguous or malformed
//!   fragment is skipped, never invented.
//! * A file whose extension is a *known IDL family we do not yet parse* is
//!   recorded in [`ContractSet::unknown_dialects`] and contributes **no** nodes
//!   — the caller degrades (`unknown-idl-dialect`) rather than guessing.
//! * Anchoring codegen to these nodes (which side implements/consumes a
//!   contract) is the caller's job (see the `shuttle-cli` contract layer); this
//!   module only produces the ground-truth node set.

use std::collections::{BTreeMap, BTreeSet};

/// Which IDL dialect a contract node came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IdlDialect {
    /// Protocol Buffers (`.proto`).
    Proto,
    /// Apache Thrift (`.thrift`).
    Thrift,
    /// OpenAPI / Swagger (`.openapi`, `.openapi.yaml`). Structural,
    /// indentation-aware parse of `components.schemas` + `paths` (G6-4).
    OpenApi,
    /// GraphQL SDL (`.graphql` / `.gql`). Root operation types are
    /// service-shaped; object/input/interface types are message-shaped (G6-4).
    GraphQl,
}

/// The role a contract node plays. A `Service` groups `Method`s; a `Message`
/// is a data type referenced by methods' request/response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContractKind {
    /// A service / interface grouping RPC methods.
    Service,
    /// A single RPC method / operation (belongs to a service).
    Method,
    /// A message / struct / schema data type.
    Message,
}

/// One node of the cross-language contract graph: a service, method, or message
/// declared by an IDL. Its [`Contract::node_id`] is the stable key both codegen
/// sides anchor to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Contract {
    /// The dialect that declared this node.
    pub dialect: IdlDialect,
    /// Whether this is a service, method, or message.
    pub kind: ContractKind,
    /// IDL namespace / package (proto `package`, thrift `namespace`). Empty when
    /// the IDL declares none — we record what we see, never a guessed default.
    pub namespace: String,
    /// The declared name (message/struct name, service name, or method name).
    pub name: String,
    /// For a [`ContractKind::Method`], the owning service; `None` otherwise.
    pub service: Option<String>,
    /// Field names of a message, or the request/response type names of a
    /// method, in declaration order. Best-effort and structural only.
    pub fields: Vec<String>,
    /// The IDL file this node was parsed from.
    pub path: String,
    /// 1-based line of the declaration header.
    pub line: usize,
}

impl Contract {
    /// The stable cross-language identity a codegen footprint anchors to. It is
    /// namespace-qualified so that two IDLs with a same-named message in
    /// different packages never collide. For a method the service is included
    /// so `UserService.GetUser` and `AdminService.GetUser` stay distinct.
    pub fn node_id(&self) -> String {
        let ns = if self.namespace.is_empty() {
            String::new()
        } else {
            format!("{}.", self.namespace)
        };
        match (self.kind, &self.service) {
            (ContractKind::Method, Some(svc)) => format!("{ns}{svc}.{}", self.name),
            _ => format!("{ns}{}", self.name),
        }
    }
}

/// The parsed contract node set over a fixed file universe, plus the set of
/// files whose IDL dialect we recognized as a family but could not parse.
#[derive(Debug, Clone, Default)]
pub struct ContractSet {
    contracts: Vec<Contract>,
    /// Files with a known-IDL-family extension we do not parse (yet). The caller
    /// degrades to `unknown-idl-dialect` for these — never guesses their shape.
    pub unknown_dialects: BTreeSet<String>,
}

impl ContractSet {
    /// Parse every IDL file in `contents` (`path → source`) into contract nodes.
    /// Non-IDL files are ignored; recognized-but-unparsed IDL families are
    /// recorded in [`ContractSet::unknown_dialects`].
    pub fn build(contents: &BTreeMap<String, String>) -> Self {
        let mut set = ContractSet::default();
        for (path, content) in contents {
            match dialect_of(path) {
                Some(IdlDialect::Proto) => resolve_idl_proto(path, content, &mut set.contracts),
                Some(IdlDialect::Thrift) => resolve_idl_thrift(path, content, &mut set.contracts),
                Some(IdlDialect::GraphQl) => resolve_idl_graphql(path, content, &mut set.contracts),
                Some(IdlDialect::OpenApi) => resolve_idl_openapi(path, content, &mut set.contracts),
                None => {
                    if is_unknown_idl_family(path) {
                        set.unknown_dialects.insert(path.clone());
                    }
                }
            }
        }
        set.contracts.sort();
        set.contracts.dedup();
        set
    }

    /// All parsed contract nodes, deterministic order.
    pub fn contracts(&self) -> &[Contract] {
        &self.contracts
    }

    /// Whether any IDL node was parsed.
    pub fn is_empty(&self) -> bool {
        self.contracts.is_empty()
    }

    /// Nodes of a given kind.
    pub fn of_kind(&self, kind: ContractKind) -> impl Iterator<Item = &Contract> {
        self.contracts.iter().filter(move |c| c.kind == kind)
    }

    /// Look up a node by its [`Contract::node_id`].
    pub fn by_node_id(&self, id: &str) -> Option<&Contract> {
        self.contracts.iter().find(|c| c.node_id() == id)
    }
}

/// Map a path to its IDL dialect by extension. `None` = not a dialect we route.
fn dialect_of(path: &str) -> Option<IdlDialect> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".proto") {
        Some(IdlDialect::Proto)
    } else if lower.ends_with(".thrift") {
        Some(IdlDialect::Thrift)
    } else if lower.ends_with(".graphql") || lower.ends_with(".gql") {
        Some(IdlDialect::GraphQl)
    } else if lower.ends_with(".openapi") || lower.ends_with(".openapi.yaml") {
        Some(IdlDialect::OpenApi)
    } else {
        None
    }
}

/// Extensions that are unmistakably an IDL family we do NOT parse. Hitting one
/// is the clean `unknown-idl-dialect` signal (§7 — degrade, never guess). We
/// deliberately keep this list to formats that are *only ever* IDLs, so a plain
/// `.json`/`.yaml` config is never mis-flagged as an unknown contract.
fn is_unknown_idl_family(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    const IDL_ONLY_EXTS: [&str; 6] = [".fbs", ".capnp", ".avdl", ".avsc", ".smithy", ".ice"];
    IDL_ONLY_EXTS.iter().any(|e| lower.ends_with(e))
}

/// Strip a `//` or `#` line comment tail; return the code part, trimmed.
fn strip_comment(line: &str) -> &str {
    let mut cut = line.len();
    if let Some(i) = line.find("//") {
        cut = cut.min(i);
    }
    if let Some(i) = line.find('#') {
        cut = cut.min(i);
    }
    line[..cut].trim()
}

/// Parse a `.proto` file into contract nodes. Line-oriented and structural: we
/// recognize `package`, `message NAME {`, `service NAME {`, `rpc M(Req) returns
/// (Resp)`, and message field lines `type name = N;`. Anything we cannot read
/// cleanly is skipped (§7).
pub fn resolve_idl_proto(path: &str, content: &str, out: &mut Vec<Contract>) {
    let mut namespace = String::new();
    // Stack of (kind, name, start-index-in-out) for the currently open block, so
    // fields/methods attach to the right parent and brace depth is tracked.
    enum Block {
        Message { idx: usize },
        Service { name: String },
    }
    let mut open: Option<Block> = None;
    for (i, raw) in content.lines().enumerate() {
        let line = strip_comment(raw);
        if line.is_empty() {
            continue;
        }
        let ln = i + 1;

        if let Some(rest) = line.strip_prefix("package ") {
            namespace = rest.trim().trim_end_matches(';').trim().to_string();
            continue;
        }

        // Closing brace ends the current block.
        if line == "}" || line.starts_with('}') {
            open = None;
            continue;
        }

        // message NAME {
        if let Some(name) = block_header(line, "message") {
            out.push(Contract {
                dialect: IdlDialect::Proto,
                kind: ContractKind::Message,
                namespace: namespace.clone(),
                name,
                service: None,
                fields: Vec::new(),
                path: path.to_string(),
                line: ln,
            });
            open = Some(Block::Message { idx: out.len() - 1 });
            continue;
        }

        // service NAME {
        if let Some(name) = block_header(line, "service") {
            out.push(Contract {
                dialect: IdlDialect::Proto,
                kind: ContractKind::Service,
                namespace: namespace.clone(),
                name: name.clone(),
                service: None,
                fields: Vec::new(),
                path: path.to_string(),
                line: ln,
            });
            open = Some(Block::Service { name });
            continue;
        }

        match &open {
            Some(Block::Service { name: svc, .. }) => {
                // rpc GetUser (GetUserRequest) returns (GetUserResponse);
                if let Some(m) = parse_proto_rpc(line) {
                    out.push(Contract {
                        dialect: IdlDialect::Proto,
                        kind: ContractKind::Method,
                        namespace: namespace.clone(),
                        name: m.0,
                        service: Some(svc.clone()),
                        fields: m.1,
                        path: path.to_string(),
                        line: ln,
                    });
                }
            }
            Some(Block::Message { idx }) => {
                // `type name = N;`  (also `repeated type name = N;`)
                if let Some(fname) = parse_proto_field(line) {
                    out[*idx].fields.push(fname);
                }
            }
            None => {}
        }
    }
}

/// `<kw> NAME {` (possibly with trailing tokens before the brace) → `NAME`.
fn block_header(line: &str, kw: &str) -> Option<String> {
    let rest = line.strip_prefix(kw)?;
    let rest = rest.strip_prefix(char::is_whitespace)?.trim_start();
    // Take the identifier up to whitespace, `{`, or `(`.
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
        .collect();
    if name.is_empty() || !line.contains('{') {
        return None;
    }
    Some(name)
}

/// Parse a proto `rpc` line → (method, [request, response]).
fn parse_proto_rpc(line: &str) -> Option<(String, Vec<String>)> {
    let rest = line.strip_prefix("rpc ")?;
    let paren = rest.find('(')?;
    let method = rest[..paren].trim().to_string();
    if method.is_empty() {
        return None;
    }
    let mut types = Vec::new();
    // request: inside the first (...)
    let after = &rest[paren + 1..];
    if let Some(end) = after.find(')') {
        let req = after[..end].trim().trim_start_matches("stream ").trim();
        if !req.is_empty() {
            types.push(req.to_string());
        }
        // response: inside "returns ( ... )"
        if let Some(ri) = after[end..].find("returns") {
            let resp_area = &after[end + ri..];
            if let (Some(op), Some(cp)) = (resp_area.find('('), resp_area.find(')')) {
                if cp > op {
                    let resp = resp_area[op + 1..cp]
                        .trim()
                        .trim_start_matches("stream ")
                        .trim();
                    if !resp.is_empty() {
                        types.push(resp.to_string());
                    }
                }
            }
        }
    }
    Some((method, types))
}

/// Parse a proto message field line `type name = N;` → the field name.
fn parse_proto_field(line: &str) -> Option<String> {
    let eq = line.find('=')?;
    let decl = line[..eq].trim();
    // decl is `[repeated|optional] type name`; the field name is the last token.
    let name = decl.split_whitespace().last()?;
    if name.chars().all(|c| c.is_alphanumeric() || c == '_') && !name.is_empty() {
        Some(name.to_string())
    } else {
        None
    }
}

/// Parse a `.thrift` file into contract nodes. Recognizes `namespace LANG NAME`,
/// `struct NAME {`, `service NAME {`, method lines `RetType name(args)`, and
/// struct field lines `N: type name`. Structural and best-effort (§7).
pub fn resolve_idl_thrift(path: &str, content: &str, out: &mut Vec<Contract>) {
    // Thrift declares a namespace per target language; we keep the first we see
    // as the node namespace (they are conventionally the same logical package).
    let mut namespace = String::new();
    enum Block {
        Struct { idx: usize },
        Service { name: String },
    }
    let mut open: Option<Block> = None;
    for (i, raw) in content.lines().enumerate() {
        let line = strip_comment(raw);
        if line.is_empty() {
            continue;
        }
        let ln = i + 1;

        if let Some(rest) = line.strip_prefix("namespace ") {
            // `namespace go user.api` → take the logical package (last token).
            if namespace.is_empty() {
                if let Some(ns) = rest.split_whitespace().nth(1) {
                    namespace = ns.to_string();
                }
            }
            continue;
        }

        if line == "}" || line.starts_with('}') {
            open = None;
            continue;
        }

        // struct/union/exception NAME {  → all are message-shaped data types.
        for kw in ["struct", "union", "exception"] {
            if let Some(name) = block_header(line, kw) {
                out.push(Contract {
                    dialect: IdlDialect::Thrift,
                    kind: ContractKind::Message,
                    namespace: namespace.clone(),
                    name,
                    service: None,
                    fields: Vec::new(),
                    path: path.to_string(),
                    line: ln,
                });
                open = Some(Block::Struct { idx: out.len() - 1 });
                break;
            }
        }
        if matches!(open, Some(Block::Struct { idx }) if out[idx].line == ln) {
            continue;
        }

        if let Some(name) = block_header(line, "service") {
            out.push(Contract {
                dialect: IdlDialect::Thrift,
                kind: ContractKind::Service,
                namespace: namespace.clone(),
                name: name.clone(),
                service: None,
                fields: Vec::new(),
                path: path.to_string(),
                line: ln,
            });
            open = Some(Block::Service { name });
            continue;
        }

        match &open {
            Some(Block::Service { name: svc }) => {
                if let Some(m) = parse_thrift_method(line) {
                    out.push(Contract {
                        dialect: IdlDialect::Thrift,
                        kind: ContractKind::Method,
                        namespace: namespace.clone(),
                        name: m.0,
                        service: Some(svc.clone()),
                        fields: m.1,
                        path: path.to_string(),
                        line: ln,
                    });
                }
            }
            Some(Block::Struct { idx }) => {
                if let Some(fname) = parse_thrift_field(line) {
                    let i = *idx;
                    out[i].fields.push(fname);
                }
            }
            None => {}
        }
    }
}

/// Parse a thrift method line `RetType methodName(args)` → (method, [ret]).
fn parse_thrift_method(line: &str) -> Option<(String, Vec<String>)> {
    let paren = line.find('(')?;
    let head = line[..paren].trim();
    let mut it = head.split_whitespace();
    let ret = it.next()?.to_string();
    let method = it.next()?.to_string();
    if method.is_empty() || !method.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let mut types = Vec::new();
    if ret != "void" {
        types.push(ret);
    }
    Some((method, types))
}

/// Parse a thrift struct field line `N: [required|optional] type name` → name.
fn parse_thrift_field(line: &str) -> Option<String> {
    let colon = line.find(':')?;
    // The part before ':' must be a field id (integer).
    if !line[..colon].trim().chars().all(|c| c.is_ascii_digit()) || line[..colon].trim().is_empty()
    {
        return None;
    }
    let decl = line[colon + 1..].trim().trim_end_matches([',', ';']).trim();
    let name = decl.split_whitespace().last()?;
    let name = name.split('=').next().unwrap_or(name).trim();
    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        Some(name.to_string())
    } else {
        None
    }
}

/// Parse a GraphQL SDL (`.graphql` / `.gql`) into contract nodes. Line-oriented
/// and structural: the root operation types `type Query|Mutation|Subscription`
/// are service-shaped and their fields become methods; every other
/// `type|input|interface NAME { ... }` is a message-shaped data type whose
/// fields we record. Enums, unions, scalars and directives are skipped — we
/// only mint nodes for shapes we can read cleanly (§7). GraphQL has no package,
/// so the namespace is always empty.
pub fn resolve_idl_graphql(path: &str, content: &str, out: &mut Vec<Contract>) {
    enum Block {
        /// A root operation type (Query/Mutation/Subscription) — service-shaped.
        Root { name: String },
        /// An object/input/interface type — message-shaped; carries its `out` idx.
        Object { idx: usize },
    }
    let mut open: Option<Block> = None;
    for (i, raw) in content.lines().enumerate() {
        let line = strip_comment(raw);
        if line.is_empty() {
            continue;
        }
        let ln = i + 1;

        if line == "}" || line.starts_with('}') {
            open = None;
            continue;
        }

        // `type NAME {`, `input NAME {`, `interface NAME {` open a block. The
        // three root operation types are service-shaped; everything else is a
        // message-shaped data type.
        let header = block_header(line, "type")
            .or_else(|| block_header(line, "input"))
            .or_else(|| block_header(line, "interface"));
        if let Some(name) = header {
            let is_root = matches!(name.as_str(), "Query" | "Mutation" | "Subscription");
            if is_root {
                out.push(Contract {
                    dialect: IdlDialect::GraphQl,
                    kind: ContractKind::Service,
                    namespace: String::new(),
                    name: name.clone(),
                    service: None,
                    fields: Vec::new(),
                    path: path.to_string(),
                    line: ln,
                });
                open = Some(Block::Root { name });
            } else {
                out.push(Contract {
                    dialect: IdlDialect::GraphQl,
                    kind: ContractKind::Message,
                    namespace: String::new(),
                    name,
                    service: None,
                    fields: Vec::new(),
                    path: path.to_string(),
                    line: ln,
                });
                open = Some(Block::Object { idx: out.len() - 1 });
            }
            continue;
        }

        match &open {
            Some(Block::Root { name: svc }) => {
                if let Some((fname, ret)) = parse_graphql_field(line) {
                    let mut fields = Vec::new();
                    if let Some(r) = ret {
                        fields.push(r);
                    }
                    out.push(Contract {
                        dialect: IdlDialect::GraphQl,
                        kind: ContractKind::Method,
                        namespace: String::new(),
                        name: fname,
                        service: Some(svc.clone()),
                        fields,
                        path: path.to_string(),
                        line: ln,
                    });
                }
            }
            Some(Block::Object { idx }) => {
                if let Some((fname, _)) = parse_graphql_field(line) {
                    let i = *idx;
                    out[i].fields.push(fname);
                }
            }
            None => {}
        }
    }
}

/// Parse a GraphQL field/operation line `name(args): ReturnType` → (name, ret).
/// The name is the leading identifier (before any `(` or `:`); the return type
/// is the identifier after the colon that follows the argument list (stripped
/// of `!`, `[`, `]` list/non-null decorators). Lines without a top-level colon
/// (descriptions, braces) yield `None`.
fn parse_graphql_field(line: &str) -> Option<(String, Option<String>)> {
    let t = line.trim();
    let paren = t.find('(');
    // The return-type colon is the one after the argument list (if any).
    let ret_colon = match paren {
        Some(op) => {
            let close = t[op..].find(')')? + op;
            t[close..].find(':').map(|c| close + c)
        }
        None => t.find(':'),
    }?;
    // The name ends at the first `(` or `:`.
    let name_end = match paren {
        Some(op) => op.min(ret_colon),
        None => ret_colon,
    };
    let name = t[..name_end].trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let ret_raw: String = t[ret_colon + 1..]
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    let ret = if ret_raw.is_empty() {
        None
    } else {
        Some(ret_raw)
    };
    Some((name.to_string(), ret))
}

/// Parse an OpenAPI / Swagger document (`.openapi`, `.openapi.yaml`) into
/// contract nodes. This is a structural, indentation-aware YAML walk — NOT a
/// full YAML parser — that recognizes exactly two shapes and skips everything
/// else (§7 — we never guess a shape we cannot read):
///
///   * **Schemas** — `components:`→`schemas:` (OpenAPI 3) or a top-level
///     `definitions:` (Swagger 2). Each schema key is a message; the keys under
///     its `properties:` are the fields.
///   * **Operations** — `paths:`→`/path:`→`get|post|put|delete|patch|...:`.
///     Each operation is a method whose service is the path string; its name is
///     the `operationId` when the operation declares one, else `VERB /path`.
pub fn resolve_idl_openapi(path: &str, content: &str, out: &mut Vec<Contract>) {
    const VERBS: [&str; 7] = ["get", "post", "put", "delete", "patch", "options", "head"];
    // A stack of (indent, key) mirroring the current YAML key path. A `key:`
    // line pops every entry at an indent >= its own before pushing itself, so
    // `stack` always spells the dotted path to the current node.
    let mut stack: Vec<(usize, String)> = Vec::new();
    // schema-name → out index, so `properties:` children attach to their owner.
    let mut schema_idx: BTreeMap<String, usize> = BTreeMap::new();
    // (path, verb) → out index, so a later `operationId:` renames its method.
    let mut op_idx: BTreeMap<(String, String), usize> = BTreeMap::new();

    for (i, raw) in content.lines().enumerate() {
        // YAML uses `#` comments only; keep `//` intact (it can appear in paths).
        let no_comment = match raw.find('#') {
            Some(c) => &raw[..c],
            None => raw,
        };
        if no_comment.trim().is_empty() {
            continue;
        }
        let indent = no_comment.len() - no_comment.trim_start().len();
        let body = no_comment.trim_end();
        let ln = i + 1;

        // Only `key:` (mapping) lines drive the walk; list items / scalars are
        // ignored for structure.
        let colon = match body.find(':') {
            Some(c) => c,
            None => continue,
        };
        let key = body[..colon].trim().trim_matches(['"', '\'']).to_string();
        let value = body[colon + 1..].trim().to_string();
        if key.is_empty() {
            continue;
        }

        // Rewind the path to this indent, then descend into `key`.
        while stack.last().map(|(ind, _)| *ind >= indent).unwrap_or(false) {
            stack.pop();
        }
        stack.push((indent, key.clone()));
        let depth = stack.len();
        let parent = if depth >= 2 {
            stack[depth - 2].1.as_str()
        } else {
            ""
        };
        let grand = if depth >= 3 {
            stack[depth - 3].1.as_str()
        } else {
            ""
        };

        // --- Schema declaration: components.schemas.NAME  or  definitions.NAME
        let is_schema = (parent == "schemas" && grand == "components")
            || (parent == "definitions" && depth == 2);
        if is_schema && is_ident(&key) && !schema_idx.contains_key(&key) {
            out.push(Contract {
                dialect: IdlDialect::OpenApi,
                kind: ContractKind::Message,
                namespace: String::new(),
                name: key.clone(),
                service: None,
                fields: Vec::new(),
                path: path.to_string(),
                line: ln,
            });
            schema_idx.insert(key.clone(), out.len() - 1);
            continue;
        }

        // --- Schema property: ...schemas.NAME.properties.FIELD
        if parent == "properties" && is_ident(&key) {
            // The owning schema is the entry two below `properties` on the path.
            if let Some(owner) = stack.get(depth.wrapping_sub(3)).map(|(_, k)| k.clone()) {
                if let Some(&idx) = schema_idx.get(&owner) {
                    out[idx].fields.push(key.clone());
                }
            }
            continue;
        }

        // --- Path operation: paths./path.VERB
        if grand == "paths" && parent.starts_with('/') && VERBS.contains(&key.as_str()) {
            let svc = parent.to_string();
            let default_name = format!("{} {}", key.to_ascii_uppercase(), svc);
            out.push(Contract {
                dialect: IdlDialect::OpenApi,
                kind: ContractKind::Method,
                namespace: String::new(),
                name: default_name,
                service: Some(svc.clone()),
                fields: Vec::new(),
                path: path.to_string(),
                line: ln,
            });
            op_idx.insert((svc, key.clone()), out.len() - 1);
            continue;
        }

        // --- operationId renames its owning operation to a stable identifier.
        if key == "operationId" && !value.is_empty() && !parent.is_empty() {
            let verb = parent.to_string();
            // The path is the entry two below the verb.
            if let Some(pth) = stack.get(depth.wrapping_sub(3)).map(|(_, k)| k.clone()) {
                if let Some(&idx) = op_idx.get(&(pth, verb)) {
                    let opid = value.trim_matches(['"', '\'']).to_string();
                    if is_ident(&opid) {
                        out[idx].name = opid;
                    }
                }
            }
            continue;
        }
    }
}

/// A plain identifier token (letters/digits/underscore, non-empty). Used to
/// gate OpenAPI schema/property/operationId keys — anything else is skipped.
fn is_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(files: &[(&str, &str)]) -> ContractSet {
        let m: BTreeMap<String, String> = files
            .iter()
            .map(|(p, c)| (p.to_string(), c.to_string()))
            .collect();
        ContractSet::build(&m)
    }

    #[test]
    fn proto_service_message_rpc_parse() {
        let src = r#"
syntax = "proto3";
package user.api;

message GetUserRequest {
  string id = 1;
  int32 page = 2;
}

message GetUserResponse {
  string name = 1;
}

service UserService {
  rpc GetUser(GetUserRequest) returns (GetUserResponse);
}
"#;
        let set = build(&[("api/user.proto", src)]);
        // messages
        let req = set.by_node_id("user.api.GetUserRequest").unwrap();
        assert_eq!(req.kind, ContractKind::Message);
        assert_eq!(req.fields, vec!["id".to_string(), "page".to_string()]);
        // service
        let svc = set.by_node_id("user.api.UserService").unwrap();
        assert_eq!(svc.kind, ContractKind::Service);
        // method — namespaced under service
        let m = set.by_node_id("user.api.UserService.GetUser").unwrap();
        assert_eq!(m.kind, ContractKind::Method);
        assert_eq!(m.service.as_deref(), Some("UserService"));
        assert_eq!(
            m.fields,
            vec!["GetUserRequest".to_string(), "GetUserResponse".to_string()]
        );
        assert!(set.unknown_dialects.is_empty());
    }

    #[test]
    fn proto_streaming_rpc_types_stripped() {
        let src = r#"
package s;
service S {
  rpc Watch(stream Req) returns (stream Resp);
}
"#;
        let set = build(&[("s.proto", src)]);
        let m = set.by_node_id("s.S.Watch").unwrap();
        assert_eq!(m.fields, vec!["Req".to_string(), "Resp".to_string()]);
    }

    #[test]
    fn thrift_struct_service_method_parse() {
        let src = r#"
namespace go user.api
namespace py user_api

struct GetUserRequest {
  1: string id
  2: optional i32 page
}

struct GetUserResponse {
  1: string name
}

service UserService {
  GetUserResponse getUser(1: GetUserRequest req)
}
"#;
        let set = build(&[("user.thrift", src)]);
        let req = set.by_node_id("user.api.GetUserRequest").unwrap();
        assert_eq!(req.kind, ContractKind::Message);
        assert_eq!(req.fields, vec!["id".to_string(), "page".to_string()]);
        let m = set.by_node_id("user.api.UserService.getUser").unwrap();
        assert_eq!(m.kind, ContractKind::Method);
        assert_eq!(m.service.as_deref(), Some("UserService"));
        assert_eq!(m.fields, vec!["GetUserResponse".to_string()]);
    }

    #[test]
    fn thrift_void_method_has_no_return_type() {
        let src = r#"
namespace go x
service Ping {
  void ping()
}
"#;
        let set = build(&[("x.thrift", src)]);
        let m = set.by_node_id("x.Ping.ping").unwrap();
        assert!(m.fields.is_empty());
    }

    #[test]
    fn unknown_idl_family_is_recorded_not_parsed() {
        // A flatbuffers schema: a known IDL family we do NOT parse.
        let set = build(&[("schema.fbs", "table Foo { x:int; }")]);
        assert!(set.is_empty(), "no nodes for an unparsed dialect");
        assert!(set.unknown_dialects.contains("schema.fbs"));
    }

    #[test]
    fn plain_config_is_not_mistaken_for_idl() {
        // A regular json/yaml config must NOT be flagged as an unknown IDL.
        let set = build(&[
            ("config.json", "{\"a\":1}"),
            ("values.yaml", "a: 1"),
            ("main.rs", "fn main() {}"),
        ]);
        assert!(set.is_empty());
        assert!(set.unknown_dialects.is_empty());
    }

    #[test]
    fn graphql_root_ops_are_services_objects_are_messages() {
        let src = r#"
type Query {
  getUser(id: ID!): User
  listUsers: [User!]!
}

type User {
  id: ID!
  name: String
}

input NewUser {
  name: String!
}
"#;
        let set = build(&[("api/schema.graphql", src)]);
        assert!(set.unknown_dialects.is_empty(), "graphql is parsed now");
        // Root operation type is service-shaped; its fields are methods.
        let q = set.by_node_id("Query").unwrap();
        assert_eq!(q.kind, ContractKind::Service);
        let m = set.by_node_id("Query.getUser").unwrap();
        assert_eq!(m.kind, ContractKind::Method);
        assert_eq!(m.service.as_deref(), Some("Query"));
        assert_eq!(m.fields, vec!["User".to_string()]);
        // A plain object type is message-shaped; scalar-typed fields recorded.
        let u = set.by_node_id("User").unwrap();
        assert_eq!(u.kind, ContractKind::Message);
        assert_eq!(u.fields, vec!["id".to_string(), "name".to_string()]);
        // input types are also message-shaped.
        let n = set.by_node_id("NewUser").unwrap();
        assert_eq!(n.kind, ContractKind::Message);
        assert_eq!(n.fields, vec!["name".to_string()]);
    }

    #[test]
    fn graphql_has_empty_namespace() {
        let set = build(&[("s.graphql", "type User { id: ID! }")]);
        let u = set.by_node_id("User").unwrap();
        assert!(u.namespace.is_empty());
        assert_eq!(u.node_id(), "User");
    }

    #[test]
    fn openapi_schemas_are_messages_with_property_fields() {
        let src = r#"
openapi: 3.0.0
info:
  title: Users
components:
  schemas:
    GetUserRequest:
      type: object
      properties:
        id:
          type: string
        page:
          type: integer
    GetUserResponse:
      type: object
      properties:
        name:
          type: string
"#;
        let set = build(&[("api/user.openapi", src)]);
        assert!(set.unknown_dialects.is_empty(), "openapi is parsed now");
        let req = set.by_node_id("GetUserRequest").unwrap();
        assert_eq!(req.kind, ContractKind::Message);
        assert_eq!(req.fields, vec!["id".to_string(), "page".to_string()]);
        let resp = set.by_node_id("GetUserResponse").unwrap();
        assert_eq!(resp.fields, vec!["name".to_string()]);
    }

    #[test]
    fn openapi_operations_use_operation_id_else_verb_path() {
        let src = r#"
openapi: 3.0.0
paths:
  /users/{id}:
    get:
      operationId: getUser
      responses:
        '200':
          description: ok
    delete:
      responses:
        '204':
          description: gone
"#;
        let set = build(&[("api/user.openapi", src)]);
        // The op with an operationId is renamed to it (node_id is service-scoped:
        // the service is the path, so the id is `/users/{id}.getUser`).
        let got = set.by_node_id("/users/{id}.getUser").unwrap();
        assert_eq!(got.kind, ContractKind::Method);
        assert_eq!(got.name, "getUser");
        assert_eq!(got.service.as_deref(), Some("/users/{id}"));
        // The op WITHOUT an operationId falls back to `VERB /path` — never guessed.
        let del = set
            .contracts()
            .iter()
            .find(|c| c.kind == ContractKind::Method && c.name.starts_with("DELETE "))
            .expect("delete op present under VERB /path name");
        assert_eq!(del.name, "DELETE /users/{id}");
        assert_eq!(del.service.as_deref(), Some("/users/{id}"));
    }

    #[test]
    fn swagger2_definitions_are_messages() {
        let src = r#"
swagger: "2.0"
definitions:
  Pet:
    type: object
    properties:
      name:
        type: string
      tag:
        type: string
"#;
        let set = build(&[("api/pet.openapi", src)]);
        let pet = set.by_node_id("Pet").unwrap();
        assert_eq!(pet.kind, ContractKind::Message);
        assert_eq!(pet.fields, vec!["name".to_string(), "tag".to_string()]);
    }

    #[test]
    fn flatbuffers_still_degrades_as_unknown_family() {
        // A genuinely-unparsed IDL family stays the clean unknown-dialect signal.
        let set = build(&[("schema.fbs", "table Foo { x:int; }")]);
        assert!(set.is_empty());
        assert!(set.unknown_dialects.contains("schema.fbs"));
    }

    #[test]
    fn no_namespace_yields_unqualified_node_id() {
        let set = build(&[("bare.proto", "message Bare { string x = 1; }")]);
        let c = set.by_node_id("Bare").unwrap();
        assert!(c.namespace.is_empty());
        assert_eq!(c.node_id(), "Bare");
    }
}

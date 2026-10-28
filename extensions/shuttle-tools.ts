import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { spawnSync } from "node:child_process";
import { readFileSync, mkdirSync, mkdtempSync, rmSync, writeFileSync, appendFileSync, statSync } from "node:fs";
import { join, isAbsolute } from "node:path";
import { tmpdir } from "node:os";
import { createHash } from "node:crypto";

const SYMBOL_FOOTPRINT_SCHEMA = "shuttle_symbol_footprint/v1";
const HYBRID_PARALLEL_ADVICE_SCHEMA = "shuttle_goal_parallel_advice/v2";
const HYBRID_DAG_POLICY_VERSION = "shuttle_hybrid_dag_policy/v1";
const PARALLEL_ADVICE_V1_SCHEMA = "shuttle_goal_parallel_advice/v1";
const DIGEST_RE = /^sha256:[0-9a-f]{64}$/;
const OID_RE = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/;
const AUTHORITY_KEYS = new Set(["approval", "claim", "credential", "dispatch", "fencing_token", "land", "push", "workspace", "write_scope"]);

/**
 * shuttle × pi tool suite (W3).
 *
 * Architecture reversal (docs/shuttle_wraps_pi_architecture.md): `shuttle` is the host and
 * pi is a replaceable LLM planner. `shuttle run` spawns pi with its built-in
 * edit/write disabled (`--exclude-tools edit,write`) and injects THIS extension
 * so every disk write and every code lookup flows through shuttle's fast Rust path:
 *
 *   · apply_edit_plan — the ONLY write path. Content-anchored search/replace /
 *     insert / create / codemod ops, applied with a fuzzy-match ladder. A
 *     deterministic dry-run gate probes anchor resolution BEFORE any disk write
 *     and, on a miss, returns a structured diagnosis so the planner fixes in one
 *     round instead of retrying blind.
 *   · shuttle_locate       — find anchors (text or structural) before planning edits.
 *   · shuttle_repomap      — token-budgeted repo map for planning context (§7.5).
 *   · shuttle_rename       — symbol-table structural rename, zero model tokens.
 *
 * Every tool shells out to the `shuttle` binary. The binary path is resolved from
 * (in order) the tool's `shuttle_bin` param, the `SHUTTLE_BIN` env var, or `shuttle` on PATH.
 * No LLM Gateway HTTP anywhere — planning is the single model inside pi. The
 * gate is pure string work (schema shape + anchor probe), NOT a second model.
 */

/** Resolve the shuttle binary path: explicit param > SHUTTLE_BIN env > `shuttle` on PATH. */
function eaBin(explicit?: string): string {
  return explicit ?? process.env.SHUTTLE_BIN ?? "shuttle";
}

/** Shape of a spawnSync failure turned into a pi tool error result. */
function launchError(bin: string, msg: string) {
  return {
    content: [
      {
        type: "text" as const,
        text: `Failed to launch \`${bin}\`: ${msg}. Is the shuttle executor built and on PATH (or SHUTTLE_BIN set)?`,
      },
    ],
    details: { launched: false },
    isError: true,
  };
}

/** Shape of a non-zero shuttle exit turned into a pi tool error result. */
function exitError(status: number | null, stderr: string) {
  return {
    content: [
      {
        type: "text" as const,
        text: `shuttle exited ${status}. stderr:\n${stderr ?? ""}`.trim(),
      },
    ],
    details: { exitCode: status, stderr },
    isError: true,
  };
}

function adapterError(message: string) {
  return {
    content: [{ type: "text" as const, text: `SHUTTLE compatibility adapter rejected payload: ${message}` }],
    details: { validated: false },
    isError: true,
  };
}

function canonicalJson(value: unknown): string {
  const canonicalize = (item: unknown): unknown => {
    if (Array.isArray(item)) return item.map(canonicalize);
    if (item !== null && typeof item === "object") {
      return Object.fromEntries(
        Object.entries(item as Record<string, unknown>)
          .sort(([left], [right]) => left < right ? -1 : left > right ? 1 : 0)
          .map(([key, child]) => [key, canonicalize(child)]),
      );
    }
    return item;
  };
  return JSON.stringify(canonicalize(value));
}

function canonicalDigest(value: unknown): string {
  return `sha256:${createHash("sha256").update(canonicalJson(value), "utf-8").digest("hex")}`;
}

function closedObject(value: unknown, required: readonly string[], optional: readonly string[], label: string): Record<string, any> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw new Error(`${label} must be an object`);
  const obj = value as Record<string, any>;
  const missing = required.filter((key) => !(key in obj));
  const unknown = Object.keys(obj).filter((key) => !required.includes(key) && !optional.includes(key));
  if (missing.length) throw new Error(`${label} is missing fields: ${missing.join(", ")}`);
  if (unknown.length) throw new Error(`${label} has unknown fields: ${unknown.join(", ")}`);
  return obj;
}

function arrayValue(value: unknown, label: string): any[] {
  if (!Array.isArray(value)) throw new Error(`${label} must be an array`);
  return value;
}

function textValue(value: unknown, label: string): string {
  if (typeof value !== "string" || value.length === 0 || value.trim() !== value || value.length > 4096 || /[\x00-\x1f\x7f]/.test(value)) {
    throw new Error(`${label} must be non-empty, trimmed and control-free`);
  }
  return value;
}

function digestValue(value: unknown, label: string): string {
  if (typeof value !== "string" || !DIGEST_RE.test(value)) throw new Error(`${label} must be sha256:<64 lowercase hex>`);
  return value;
}

function oidValue(value: unknown, label: string): string {
  if (typeof value !== "string" || !OID_RE.test(value)) throw new Error(`${label} must be a full lowercase hex OID`);
  return value;
}

function enumValue<T extends string>(value: unknown, allowed: readonly T[], label: string): T {
  if (typeof value !== "string" || !allowed.includes(value as T)) throw new Error(`${label} has unsupported value: ${String(value)}`);
  return value as T;
}

function compareOrder(left: unknown, right: unknown): number {
  if (typeof left === "number" && typeof right === "number") return left - right;
  if (typeof left === "string" && typeof right === "string") return Buffer.compare(Buffer.from(left), Buffer.from(right));
  if (Array.isArray(left) && Array.isArray(right)) {
    for (let index = 0; index < Math.min(left.length, right.length); index++) {
      const compared = compareOrder(left[index], right[index]);
      if (compared !== 0) return compared;
    }
    return left.length - right.length;
  }
  return Buffer.compare(Buffer.from(canonicalJson(left)), Buffer.from(canonicalJson(right)));
}

function sortedUnique<T>(values: T[], label: string, key: (value: T) => unknown = (value) => canonicalJson(value)): void {
  const keys = values.map(key);
  for (let index = 1; index < keys.length; index++) {
    if (compareOrder(keys[index - 1], keys[index]) >= 0) throw new Error(`${label} must be strictly sorted and unique`);
  }
}

const EVIDENCE_KIND_ORDER: Record<string, number> = { import_directed: 0, contract_directed: 1, name_match: 2, co_change: 3 };
const CONFLICT_KIND_ORDER: Record<string, number> = { reverse_model_edge: 0, cycle_producing_addition: 1, model_cycle: 2, write_write_collision: 3, ambiguous_evidence: 4, invalid_task_reference: 5 };
const PROVENANCE_ORDER: Record<string, number> = { model: 0, trusted_mechanical: 1, both: 2 };
const UNRESOLVED_REASON_ORDER: Record<string, number> = { ambiguous: 0, unsupported: 1, stale: 2, absent: 3, timeout: 4, overflow: 5, invalid_declaration: 6, not_found: 7 };
function orderKey(value: unknown): unknown { return value; }
function optionOrder(value: unknown, key: (item: any) => unknown = (item) => item): unknown[] { return value === undefined ? [0] : [1, key(value)]; }
function symbolOrder(item: Record<string, any>): unknown { return orderKey([item.symbol_id, item.repo_id, item.base_oid, item.language, item.normalized_path, item.name, item.kind, optionOrder(item.scope), item.line, item.signature_digest]); }
function candidateOrder(item: Record<string, any>): unknown { return orderKey([[item.edge.from, item.edge.to], item.kinds.map((kind: string) => EVIDENCE_KIND_ORDER[kind]), item.evidence_refs]); }
function weakHintOrder(item: Record<string, any>): unknown { return orderKey([item.task_a, item.task_b, EVIDENCE_KIND_ORDER[item.kind], item.evidence_ref]); }
function conflictOrder(item: Record<string, any>): unknown { return orderKey([CONFLICT_KIND_ORDER[item.kind], item.task_ids, optionOrder(item.edge, (edge) => [edge.from, edge.to]), optionOrder(item.symbol_id), item.evidence_refs ?? []]); }
function hardEdgeOrder(item: Record<string, any>): unknown { return orderKey([[item.edge.from, item.edge.to], PROVENANCE_ORDER[item.provenance], (item.kinds ?? []).map((kind: string) => EVIDENCE_KIND_ORDER[kind]), item.evidence_refs ?? []]); }

function validateNoAuthority(value: unknown, label = "payload"): void {
  if (Array.isArray(value)) { value.forEach((item, index) => validateNoAuthority(item, `${label}[${index}]`)); return; }
  if (value === null || typeof value !== "object") return;
  for (const [key, child] of Object.entries(value as Record<string, unknown>)) {
    if (AUTHORITY_KEYS.has(key)) throw new Error(`${label} contains forbidden authority field: ${key}`);
    validateNoAuthority(child, `${label}.${key}`);
  }
}

function validateSymbolIdentity(value: unknown, label: string): Record<string, any> {
  const obj = closedObject(value, ["symbol_id", "repo_id", "base_oid", "language", "normalized_path", "name", "kind", "line", "signature_digest"], ["scope"], label);
  digestValue(obj.symbol_id, `${label}.symbol_id`);
  textValue(obj.repo_id, `${label}.repo_id`);
  oidValue(obj.base_oid, `${label}.base_oid`);
  textValue(obj.language, `${label}.language`);
  const path = textValue(obj.normalized_path, `${label}.normalized_path`);
  if (path.startsWith("/") || path.includes("\\") || path.split("/").some((part) => part === "" || part === "." || part === "..") || /^[A-Za-z]:/.test(path)) {
    throw new Error(`${label}.normalized_path must be normalized and repository-relative`);
  }
  textValue(obj.name, `${label}.name`);
  textValue(obj.kind, `${label}.kind`);
  if ("scope" in obj) textValue(obj.scope, `${label}.scope`);
  if (!Number.isInteger(obj.line) || obj.line < 1) throw new Error(`${label}.line must be a one-based integer`);
  digestValue(obj.signature_digest, `${label}.signature_digest`);
  const { symbol_id: _symbolId, ...identity } = obj;
  if (obj.symbol_id !== canonicalDigest(identity)) throw new Error(`${label}.symbol_id does not match its canonical typed identity`);
  return obj;
}

function validateUnresolved(value: unknown, label: string): Record<string, any> {
  const obj = closedObject(value, ["declaration", "reason"], [], label);
  textValue(obj.declaration, `${label}.declaration`);
  enumValue(obj.reason, ["ambiguous", "unsupported", "stale", "absent", "timeout", "overflow", "invalid_declaration", "not_found"], `${label}.reason`);
  return obj;
}

function validateSymbolList(value: unknown, label: string, repositories: Set<string>): Record<string, any>[] {
  const symbols = arrayValue(value, label);
  symbols.forEach((symbol, index) => {
    const checked = validateSymbolIdentity(symbol, `${label}[${index}]`);
    if (!repositories.has(`${checked.repo_id}\0${checked.base_oid}`)) throw new Error(`${label}[${index}] has no matching repository binding`);
  });
  sortedUnique(symbols, label, symbolOrder);
  return symbols;
}

function validateSymbolFootprint(value: unknown): Record<string, any> {
  const obj = closedObject(value, ["schema_version", "basis", "tasks"], [], "symbol_footprint");
  if (obj.schema_version !== SYMBOL_FOOTPRINT_SCHEMA) throw new Error(`unsupported symbol footprint schema_version: ${String(obj.schema_version)}`);
  validateNoAuthority(obj, "symbol_footprint");
  const basis = closedObject(obj.basis, ["repositories", "symbol_index_ref", "symbol_index_digest", "analyzer_version", "coverage", "freshness", "truncated"], ["degraded_reasons"], "symbol_footprint.basis");
  const repositories = arrayValue(basis.repositories, "repositories");
  if (!repositories.length) throw new Error("repositories must not be empty");
  const bindings = new Set<string>();
  repositories.forEach((repository) => {
    const checked = closedObject(repository, ["repo_id", "base_oid"], [], "repository");
    const key = `${textValue(checked.repo_id, "repository.repo_id")}\0${oidValue(checked.base_oid, "repository.base_oid")}`;
    if (bindings.has(key)) throw new Error("repository bindings must be unique");
    bindings.add(key);
  });
  sortedUnique(repositories, "repositories", (item) => `${item.repo_id}\0${item.base_oid}`);
  textValue(basis.symbol_index_ref, "symbol_index_ref");
  digestValue(basis.symbol_index_digest, "symbol_index_digest");
  textValue(basis.analyzer_version, "analyzer_version");
  const coverage = enumValue(basis.coverage, ["none", "partial", "exact"], "coverage");
  const freshness = enumValue(basis.freshness, ["fresh", "stale", "absent", "unsupported", "timeout"], "freshness");
  if (typeof basis.truncated !== "boolean") throw new Error("truncated must be boolean");
  const reasons = arrayValue(basis.degraded_reasons ?? [], "degraded_reasons");
  reasons.forEach((reason) => textValue(reason, "degraded_reason"));
  sortedUnique(reasons, "degraded_reasons", String);
  if (coverage === "exact" && (freshness !== "fresh" || basis.truncated)) throw new Error("exact coverage requires a fresh, non-truncated symbol index");
  if ((freshness !== "fresh" || basis.truncated) && !reasons.length) throw new Error("non-fresh or truncated footprint requires degraded_reasons");

  const tasks = arrayValue(obj.tasks, "symbol_footprint.tasks");
  tasks.forEach((task, index) => {
    const label = `symbol_footprint.tasks[${index}]`;
    const checked = closedObject(task, ["task_id", "planned", "observed", "grants_path_authority"], [], label);
    textValue(checked.task_id, `${label}.task_id`);
    if (checked.grants_path_authority !== false) throw new Error(`${label}.grants_path_authority must be false`);
    const planned = closedObject(checked.planned, ["evidence_source", "declared_read_symbols", "declared_write_symbols", "unresolved"], [], `${label}.planned`);
    const source = enumValue(planned.evidence_source, ["planner_declared_validated", "planner_declared_unresolved", "planner_guess"], `${label}.planned.evidence_source`);
    const reads = validateSymbolList(planned.declared_read_symbols, `${label}.planned.declared_read_symbols`, bindings);
    const writes = validateSymbolList(planned.declared_write_symbols, `${label}.planned.declared_write_symbols`, bindings);
    const unresolved = arrayValue(planned.unresolved, `${label}.planned.unresolved`);
    unresolved.forEach((item, offset) => validateUnresolved(item, `${label}.planned.unresolved[${offset}]`));
    sortedUnique(unresolved, `${label}.planned.unresolved`, (item) => orderKey([item.declaration, UNRESOLVED_REASON_ORDER[item.reason]]));
    if (source === "planner_declared_validated" && unresolved.length) throw new Error("planner_declared_validated must not contain unresolved declarations");
    if (source === "planner_declared_unresolved" && (!unresolved.length || reads.length || writes.length)) throw new Error("planner_declared_unresolved must contain only unresolved declarations");
    if (source === "planner_guess" && (reads.length || writes.length)) throw new Error("planner_guess must not contain exact symbols");
    const observed = closedObject(checked.observed, ["evidence_source", "observed_write_symbols", "unresolved"], [], `${label}.observed`);
    const observedSource = enumValue(observed.evidence_source, ["not_observed", "observed_diff"], `${label}.observed.evidence_source`);
    const observedWrites = validateSymbolList(observed.observed_write_symbols, `${label}.observed.observed_write_symbols`, bindings);
    const observedUnresolved = arrayValue(observed.unresolved, `${label}.observed.unresolved`);
    observedUnresolved.forEach((item, offset) => validateUnresolved(item, `${label}.observed.unresolved[${offset}]`));
    sortedUnique(observedUnresolved, `${label}.observed.unresolved`, (item) => orderKey([item.declaration, UNRESOLVED_REASON_ORDER[item.reason]]));
    if (observedSource === "not_observed" && (observedWrites.length || observedUnresolved.length)) throw new Error("not_observed must not contain observed evidence");
  });
  sortedUnique(tasks, "symbol_footprint.tasks", (task) => String(task.task_id));
  return obj;
}

function validateArtifactBinding(value: unknown, label: string): Record<string, any> {
  const obj = closedObject(value, ["ref", "digest"], [], label);
  const digest = digestValue(obj.digest, `${label}.digest`);
  if (obj.ref !== `artifact://sha256/${digest.slice("sha256:".length)}`) throw new Error(`${label}.ref must be the content address of its digest`);
  return obj;
}

function validateEdge(value: unknown, label: string): Record<string, any> {
  const obj = closedObject(value, ["from", "to"], [], label);
  textValue(obj.from, `${label}.from`); textValue(obj.to, `${label}.to`);
  if (obj.from === obj.to) throw new Error(`${label} must not be a self edge`);
  return obj;
}

function edgeKey(edge: Record<string, any>): string { return `${edge.from}\0${edge.to}`; }

function validateHybridAdvice(value: unknown, expectedFootprintDigest?: string): Record<string, any> {
  const fields = ["schema_version", "policy_version", "planning_basis", "source_intent", "repository_bases", "symbol_footprint", "symbol_index_bindings", "model_edges", "trusted_addition_candidates", "weak_hints", "conflicts", "proposed_hard_edges", "schedule", "degraded"] as const;
  const obj = closedObject(value, fields, [], "parallel_advice");
  if (obj.schema_version !== HYBRID_PARALLEL_ADVICE_SCHEMA) throw new Error(`unsupported advice schema_version: ${String(obj.schema_version)}`);
  if (obj.policy_version !== HYBRID_DAG_POLICY_VERSION) throw new Error(`unsupported policy_version: ${String(obj.policy_version)}`);
  validateNoAuthority(obj, "parallel_advice");
  const planning = closedObject(obj.planning_basis, ["planning_ref", "planning_revision", "planning_structure_digest"], [], "planning_basis");
  textValue(planning.planning_ref, "planning_ref"); textValue(planning.planning_revision, "planning_revision"); digestValue(planning.planning_structure_digest, "planning_structure_digest");
  validateArtifactBinding(obj.source_intent, "source_intent");
  const footprintBinding = validateArtifactBinding(obj.symbol_footprint, "symbol_footprint");
  if (expectedFootprintDigest && footprintBinding.digest !== expectedFootprintDigest) throw new Error("symbol_footprint.digest does not match the canonical input footprint");
  const repositories = arrayValue(obj.repository_bases, "repository_bases");
  if (!repositories.length) throw new Error("repository_bases must not be empty");
  const repoIds = new Set<string>();
  repositories.forEach((repository) => {
    const checked = closedObject(repository, ["repo_id", "base_oid"], [], "repository_base");
    const repoId = textValue(checked.repo_id, "repository_base.repo_id"); oidValue(checked.base_oid, "repository_base.base_oid");
    if (repoIds.has(repoId)) throw new Error("repository_bases repo_id must be unique"); repoIds.add(repoId);
  });
  sortedUnique(repositories, "repository_bases", (item) => [item.repo_id, item.base_oid]);
  const indexes = arrayValue(obj.symbol_index_bindings, "symbol_index_bindings");
  indexes.forEach((binding) => {
    const checked = closedObject(binding, ["repo_id", "index_digest"], [], "symbol_index_binding");
    if (!repoIds.has(textValue(checked.repo_id, "symbol_index_binding.repo_id"))) throw new Error("symbol index binding has no repository base");
    digestValue(checked.index_digest, "symbol_index_binding.index_digest");
  });
  sortedUnique(indexes, "symbol_index_bindings", (item) => [item.repo_id, item.index_digest]);
  const modelEdges = arrayValue(obj.model_edges, "model_edges");
  modelEdges.forEach((edge, index) => validateEdge(edge, `model_edges[${index}]`)); sortedUnique(modelEdges, "model_edges", (edge) => [edge.from, edge.to]);
  const model = new Set(modelEdges.map(edgeKey));
  const candidates = arrayValue(obj.trusted_addition_candidates, "trusted_addition_candidates");
  const candidateEdges = new Set<string>();
  candidates.forEach((candidate) => {
    const checked = closedObject(candidate, ["edge", "kinds", "evidence_refs"], [], "trusted_candidate");
    candidateEdges.add(edgeKey(validateEdge(checked.edge, "trusted_candidate.edge")));
    const kinds = arrayValue(checked.kinds, "trusted_candidate.kinds");
    if (!kinds.length || kinds.some((kind) => kind !== "import_directed" && kind !== "contract_directed")) throw new Error("trusted candidates require trusted directed kinds");
    sortedUnique(kinds, "trusted_candidate.kinds", String);
    const refs = arrayValue(checked.evidence_refs, "trusted_candidate.evidence_refs"); if (!refs.length) throw new Error("trusted candidates require evidence_refs");
    refs.forEach((ref) => textValue(ref, "trusted_candidate.evidence_ref")); sortedUnique(refs, "trusted_candidate.evidence_refs", String);
  });
  sortedUnique(candidates, "trusted_addition_candidates", candidateOrder);
  const weakHints = arrayValue(obj.weak_hints, "weak_hints");
  weakHints.forEach((hint) => {
    const checked = closedObject(hint, ["task_a", "task_b", "kind", "evidence_ref"], [], "weak_hint");
    const taskA = textValue(checked.task_a, "weak_hint.task_a"); const taskB = textValue(checked.task_b, "weak_hint.task_b");
    if (taskA >= taskB || (checked.kind !== "name_match" && checked.kind !== "co_change")) throw new Error("weak hints require a sorted task pair and weak kind");
    textValue(checked.evidence_ref, "weak_hint.evidence_ref");
  });
  sortedUnique(weakHints, "weak_hints", weakHintOrder);
  const conflicts = arrayValue(obj.conflicts, "conflicts");
  const conflictKinds = ["reverse_model_edge", "cycle_producing_addition", "model_cycle", "write_write_collision", "ambiguous_evidence", "invalid_task_reference"] as const;
  conflicts.forEach((conflict) => {
    const checked = closedObject(conflict, ["kind", "task_ids"], ["edge", "symbol_id", "evidence_refs"], "conflict");
    enumValue(checked.kind, conflictKinds, "conflict.kind");
    const taskIds = arrayValue(checked.task_ids, "conflict.task_ids"); taskIds.forEach((task) => textValue(task, "conflict.task_id")); sortedUnique(taskIds, "conflict.task_ids", String);
    if (checked.edge) validateEdge(checked.edge, "conflict.edge"); if (checked.symbol_id) textValue(checked.symbol_id, "conflict.symbol_id");
    const refs = arrayValue(checked.evidence_refs ?? [], "conflict.evidence_refs"); refs.forEach((ref) => textValue(ref, "conflict.evidence_ref")); sortedUnique(refs, "conflict.evidence_refs", String);
  });
  sortedUnique(conflicts, "conflicts", conflictOrder);
  const hardEdges = arrayValue(obj.proposed_hard_edges, "proposed_hard_edges");
  const hard = new Set<string>();
  hardEdges.forEach((item) => {
    const checked = closedObject(item, ["edge", "provenance"], ["kinds", "evidence_refs"], "hard_edge");
    const key = edgeKey(validateEdge(checked.edge, "hard_edge.edge"));
    const kinds = arrayValue(checked.kinds ?? [], "hard_edge.kinds"); const refs = arrayValue(checked.evidence_refs ?? [], "hard_edge.evidence_refs");
    if (kinds.some((kind) => kind !== "import_directed" && kind !== "contract_directed")) throw new Error("weak evidence must never enter proposed_hard_edges");
    sortedUnique(kinds, "hard_edge.kinds", String); refs.forEach((ref) => textValue(ref, "hard_edge.evidence_ref")); sortedUnique(refs, "hard_edge.evidence_refs", String);
    const provenance = enumValue(checked.provenance, ["model", "trusted_mechanical", "both"], "hard_edge.provenance");
    const inModel = model.has(key); const inCandidate = candidateEdges.has(key);
    const valid = (provenance === "model" && inModel && !inCandidate && !kinds.length && !refs.length)
      || (provenance === "trusted_mechanical" && !inModel && inCandidate && !!kinds.length && !!refs.length)
      || (provenance === "both" && inModel && inCandidate && !!kinds.length && !!refs.length);
    if (!valid) throw new Error("hard edge provenance is inconsistent"); hard.add(key);
  });
  sortedUnique(hardEdges, "proposed_hard_edges", hardEdgeOrder);
  for (const edge of model) if (!hard.has(edge)) throw new Error("every model edge must remain in proposed_hard_edges");
  const schedule = closedObject(obj.schedule, ["batches", "max_width", "degraded"], ["serialized"], "schedule");
  const batches = arrayValue(schedule.batches, "schedule.batches"); const batchOf = new Map<string, number>();
  batches.forEach((batch, position) => {
    const checked = closedObject(batch, ["index", "tasks"], [], "schedule.batch"); const tasks = arrayValue(checked.tasks, "schedule.batch.tasks");
    if (checked.index !== position || !tasks.length) throw new Error("schedule batches must be non-empty and contiguously indexed");
    tasks.forEach((task) => { textValue(task, "schedule.task"); if (batchOf.has(task)) throw new Error("schedule task must occur exactly once"); batchOf.set(task, position); }); sortedUnique(tasks, "schedule.batch.tasks", String);
  });
  const width = Math.max(0, ...batches.map((batch) => batch.tasks.length));
  if (!Number.isInteger(schedule.max_width) || schedule.max_width !== width || typeof schedule.degraded !== "boolean") throw new Error("schedule max_width/degraded fields are invalid");
  for (const key of hard) { const [from, to] = key.split("\0"); if (!batchOf.has(from) || !batchOf.has(to) || batchOf.get(from)! >= batchOf.get(to)!) throw new Error("schedule violates a hard edge"); }
  const serialized = arrayValue(schedule.serialized ?? [], "schedule.serialized");
  serialized.forEach((reason) => { const checked = closedObject(reason, ["earlier", "later", "reason"], [], "serialized_reason"); textValue(checked.reason, "serialized_reason.reason"); if (!batchOf.has(checked.earlier) || !batchOf.has(checked.later) || batchOf.get(checked.earlier)! >= batchOf.get(checked.later)!) throw new Error("serialized reason must point to a later batch"); }); sortedUnique(serialized, "schedule.serialized", (item) => [item.earlier, item.later, item.reason]);
  const degraded = closedObject(obj.degraded, ["value", "reasons"], [], "degraded"); const reasons = arrayValue(degraded.reasons, "degraded.reasons");
  if (typeof degraded.value !== "boolean" || degraded.value !== !!reasons.length) throw new Error("degraded.value must exactly reflect degraded.reasons");
  reasons.forEach((reason) => textValue(reason, "degraded.reason")); sortedUnique(reasons, "degraded.reasons", String);
  if ((conflicts.length || schedule.degraded) && !degraded.value) throw new Error("conflicts or conservative scheduling require degraded=true");
  return obj;
}

function validateV1Advice(value: unknown): Record<string, any> {
  const obj = closedObject(value, ["schema_version", "source_intent_ref", "source_intent_digest", "graph_basis", "batches", "serialized", "degraded", "max_width"], [], "parallel_advice_v1");
  if (obj.schema_version !== PARALLEL_ADVICE_V1_SCHEMA) throw new Error(`unsupported advice schema_version: ${String(obj.schema_version)}`);
  validateNoAuthority(obj, "parallel_advice_v1"); textValue(obj.source_intent_ref, "source_intent_ref"); digestValue(obj.source_intent_digest, "source_intent_digest");
  const basis = closedObject(obj.graph_basis, ["base_oid", "symbol_index_digest", "correction_digest", "footprint_source"], [], "graph_basis");
  oidValue(basis.base_oid, "graph_basis.base_oid"); digestValue(basis.symbol_index_digest, "graph_basis.symbol_index_digest"); digestValue(basis.correction_digest, "graph_basis.correction_digest");
  if (basis.footprint_source !== "planner_guess") throw new Error("v1 footprint_source must remain planner_guess");
  const batches = arrayValue(obj.batches, "batches"); const batchOf = new Map<string, number>();
  batches.forEach((batch, position) => {
    const checked = closedObject(batch, ["index", "task_ids"], [], "batch"); const tasks = arrayValue(checked.task_ids, "batch.task_ids");
    if (checked.index !== position || !tasks.length) throw new Error("v1 batches must be non-empty and contiguously indexed");
    tasks.forEach((task) => { textValue(task, "batch.task_id"); if (batchOf.has(task)) throw new Error("v1 task_id must occur exactly once"); batchOf.set(task, position); });
  });
  const width = Math.max(0, ...batches.map((batch) => batch.task_ids.length));
  if (!Number.isInteger(obj.max_width) || obj.max_width !== width || typeof obj.degraded !== "boolean") throw new Error("v1 max_width/degraded fields are invalid");
  const serialized = arrayValue(obj.serialized, "serialized"); let expectedDegraded = false;
  serialized.forEach((reason) => {
    const checked = closedObject(reason, ["earlier", "later", "reason"], [], "serialized_reason"); textValue(checked.reason, "serialized_reason.reason");
    if (!batchOf.has(checked.earlier) || !batchOf.has(checked.later) || batchOf.get(checked.earlier)! >= batchOf.get(checked.later)!) throw new Error("v1 serialized reason must point to a later batch");
    expectedDegraded ||= checked.reason.startsWith("unprovable-independence");
  });
  if (obj.degraded !== expectedDegraded) throw new Error("v1 degraded must exactly reflect unprovable serialization");
  return obj;
}

// ---------------------------------------------------------------------------
// G1: write-path telemetry — quantify how often the planner hand-plans an
// apply_edit_plan patch when a NAMED apply_refactor operator could have done
// it (the fallback-rate that F1/F2 aim to drive down). OFF by default; set
// SHUTTLE_TELEMETRY=<path> to append one JSONL line per write-tool call. Best-effort
// and fail-open: a telemetry error must NEVER perturb the actual edit result.
// ---------------------------------------------------------------------------
function recordTelemetry(event: {
  tool: "apply_edit_plan" | "apply_refactor" | "apply_recipe";
  op?: string;
  opCount: number;
  outcome: string;
  confidence?: string;
  warnings?: number;
  wrote?: boolean;
  error?: boolean;
}): void {
  const dest = process.env.SHUTTLE_TELEMETRY;
  if (!dest) return;
  try {
    const line = JSON.stringify({ ts: new Date().toISOString(), ...event }) + "\n";
    appendFileSync(dest, line, { encoding: "utf-8" });
  } catch {
    /* fail-open: telemetry is diagnostics, never a write-path dependency */
  }
}

// ---------------------------------------------------------------------------
// Dry-run gate helpers — deterministic, zero-model. Given a failed op we point
// the planner at the nearest real lines so it re-anchors in ONE round.
// ---------------------------------------------------------------------------

/** Collapse whitespace runs + trim, so tab/space drift never hides a match. */
function normWs(s: string): string {
  return s.replace(/\s+/g, " ").trim();
}

/** First non-empty, whitespace-normalized line of a multi-line anchor. */
function anchorHeadLine(anchor: string): string {
  for (const raw of anchor.split("\n")) {
    const n = normWs(raw);
    if (n.length > 0) return n;
  }
  return normWs(anchor);
}

/** Bigram set of a normalized string (for a cheap Dice similarity). */
function bigrams(s: string): Set<string> {
  const g = new Set<string>();
  const t = s.length < 2 ? s : s;
  for (let i = 0; i < t.length - 1; i++) g.add(t.slice(i, i + 2));
  if (t.length === 1) g.add(t);
  return g;
}

/** Dice coefficient over character bigrams — 1.0 identical, 0.0 disjoint. */
function diceSim(a: string, b: string): number {
  if (a === b) return 1;
  const A = bigrams(a);
  const B = bigrams(b);
  if (A.size === 0 || B.size === 0) return 0;
  let inter = 0;
  for (const x of A) if (B.has(x)) inter++;
  return (2 * inter) / (A.size + B.size);
}

/** Map a file extension to the ast-grep `--lang` word shuttle/ast-grep expects. */
function langForPath(path: string): string | null {
  const m = path.toLowerCase().match(/\.([a-z0-9]+)$/);
  if (!m) return null;
  const ext = m[1];
  const table: Record<string, string> = {
    ts: "ts", tsx: "tsx", mts: "ts", cts: "ts",
    js: "js", jsx: "js", mjs: "js", cjs: "js",
    py: "python", rs: "rust", go: "go", java: "java",
    c: "c", h: "c", cc: "cpp", cpp: "cpp", hpp: "cpp",
    rb: "ruby", php: "php", cs: "csharp", kt: "kotlin", swift: "swift",
  };
  return table[ext] ?? null;
}

/** Pull identifier-looking tokens out of an anchor head line, longest first. */
function extractIdentifiers(line: string): string[] {
  const toks = line.match(/[A-Za-z_$][A-Za-z0-9_$]*/g) ?? [];
  const kw = new Set([
    "const", "let", "var", "function", "return", "if", "else", "for", "while",
    "class", "export", "import", "from", "new", "async", "await", "public",
    "private", "static", "def", "self", "fn", "pub", "type", "interface",
    "void", "int", "string", "number", "boolean", "true", "false", "null",
  ]);
  const seen = new Set<string>();
  const out: string[] = [];
  for (const t of toks) {
    if (kw.has(t) || t.length < 3 || seen.has(t)) continue;
    seen.add(t);
    out.push(t);
  }
  out.sort((a, b) => b.length - a.length);
  return out;
}

/** Parse `shuttle locate` output lines (`path:line:col: text`) into structured hits. */
function parseLocate(stdout: string) {
  const hits: Array<{ path: string; line: number; col: number; text: string }> = [];
  for (const raw of stdout.split("\n")) {
    const l = raw.trim();
    if (!l) continue;
    const m = l.match(/^(.*?):(\d+):(\d+):\s?(.*)$/);
    if (!m) continue;
    hits.push({ path: m[1], line: Number(m[2]), col: Number(m[3]), text: m[4] });
  }
  return hits;
}

/**
 * Structural re-anchor: run `shuttle locate` (ast-grep when a lang is known, text
 * otherwise) for the most salient identifier in the failed anchor, and keep the
 * hits inside the target file. Zero model tokens — just the shuttle binary.
 */
function structuralLocate(
  bin: string,
  root: string,
  path: string,
  anchorHead: string,
): { backend: "ast-grep" | "text" | "none"; ident: string | null; hits: Array<{ line: number; col: number; text: string }> } {
  const idents = extractIdentifiers(anchorHead);
  if (idents.length === 0) return { backend: "none", ident: null, hits: [] };
  const lang = langForPath(path);
  const target = path.replace(/^\.\//, "");

  // W-D batch: hand EVERY candidate identifier to ONE `shuttle locate` process
  // (repeated --pattern) instead of cold-starting one process per ident. shuttle
  // folds text patterns into a single ripgrep alternation; a structural (lang)
  // search loops in-process — either way the caller pays one shuttle cold-start, not
  // N. Salience is then re-applied locally: idents are longest-first, so the
  // first with an in-file hit wins, matching the old first-hit contract.
  const args = ["locate", "--root", root];
  for (const ident of idents) args.push("--pattern", ident);
  let backend: "ast-grep" | "text" = "text";
  if (lang) {
    args.push("--lang", lang);
    backend = "ast-grep";
  }
  const res = spawnSync(bin, args, { encoding: "utf-8", maxBuffer: 32 * 1024 * 1024 });
  if (res.error || res.status !== 0) return { backend: "none", ident: idents[0] ?? null, hits: [] };

  const inFile = parseLocate(res.stdout).filter(
    (h) => h.path.replace(/^\.\//, "").endsWith(target),
  );
  if (inFile.length === 0) return { backend: "none", ident: idents[0] ?? null, hits: [] };

  for (const ident of idents) {
    const forIdent = inFile
      .filter((h) => h.text.includes(ident))
      .slice(0, 5)
      .map((h) => ({ line: h.line, col: h.col, text: h.text }));
    if (forIdent.length > 0) return { backend, ident, hits: forIdent };
  }
  // Hits exist but none textually attributes to a specific ident (e.g. a
  // structural match whose line text was trimmed): surface them under the most
  // salient candidate rather than dropping a real signal.
  const anyHits = inFile.slice(0, 5).map((h) => ({ line: h.line, col: h.col, text: h.text }));
  return { backend, ident: idents[0] ?? null, hits: anyHits };
}

// ---------------------------------------------------------------------------
// (1) Run-scoped result cache. Keyed by (repo root, git HEAD oid + dirty-file
// signature), so within one `shuttle run` the planner's repeated repomap /
// context-pack / dry-run-rename calls reuse the first result instead of
// re-spawning shuttle and re-scanning ctags. Invalidation is conservative: any HEAD
// move or any change to a tracked file's size/mtime yields a fresh key, so a
// stale cache is never read. Set SHUTTLE_NO_CACHE=1 to disable. Cache lives in
// <root>/.shuttle/cache (gitignored).
// ---------------------------------------------------------------------------

/** Cache directory for a given repo root. */
function runCacheDir(root: string): string {
  return join(root, ".shuttle", "cache");
}

/**
 * Conservative signature of the repo's current state: git HEAD oid plus, for
 * every dirty (tracked-or-untracked) path, its size+mtime. Returns "" when git
 * is unavailable, so the caller skips caching rather than risk a wrong hit.
 */
function gitStateKey(root: string): string {
  const head = spawnSync("git", ["-C", root, "rev-parse", "HEAD"], { encoding: "utf-8" });
  if (head.error) return "";
  const oid = head.status === 0 ? head.stdout.trim() : "no-head";
  const st = spawnSync("git", ["-C", root, "status", "--porcelain"], { encoding: "utf-8" });
  const dirty = !st.error && st.status === 0 ? st.stdout : "";
  const parts = [oid];
  for (const line of dirty.split("\n")) {
    const p = line.slice(3).trim();
    if (!p) continue;
    try {
      const s = statSync(resolvePath(root, p));
      parts.push(`${p}:${s.size}:${Math.floor(s.mtimeMs)}`);
    } catch {
      parts.push(`${p}:gone`);
    }
  }
  return createHash("sha256").update(parts.join("\n")).digest("hex");
}

/**
 * Run a producer through the run-scoped cache. On a hit the cached tool result
 * is returned (annotated cache:"hit"); on a miss the producer runs and any
 * non-error result is persisted (annotated cache:"miss"). Errors are never
 * cached, and any cache IO failure degrades silently to just running produce().
 */
function withRunCache<T extends { isError?: boolean; details?: Record<string, unknown> }>(
  root: string,
  toolName: string,
  keyObj: unknown,
  produce: () => T,
): T {
  if (process.env.SHUTTLE_NO_CACHE === "1") return produce();
  const state = gitStateKey(root);
  if (!state) return produce();
  const dir = runCacheDir(root);
  const file = join(dir, `${state}.json`);
  const sig = createHash("sha256").update(`${toolName} ${JSON.stringify(keyObj)}`).digest("hex");

  let store: Record<string, T> = {};
  try {
    store = JSON.parse(readFileSync(file, "utf-8")) as Record<string, T>;
  } catch {
    store = {};
  }
  if (store && Object.prototype.hasOwnProperty.call(store, sig)) {
    const cached = store[sig];
    return { ...cached, details: { ...(cached.details ?? {}), cache: "hit" } };
  }

  const result = produce();
  if (result && !result.isError) {
    try {
      mkdirSync(dir, { recursive: true });
      store[sig] = result;
      writeFileSync(file, JSON.stringify(store));
    } catch {
      // best-effort cache; never fail the tool on a cache write error.
    }
  }
  return { ...result, details: { ...(result.details ?? {}), cache: "miss" } };
}

/** Resolve an op path against the apply root the same way `shuttle apply` does. */
function resolvePath(root: string, p: string): string {
  return isAbsolute(p) ? p : join(root, p);
}

/**
 * Read the target file and return the lines most similar to the anchor's head
 * line, plus any lines that match it exactly (ambiguity evidence). All work is
 * plain string comparison — no model, no network.
 */
function nearestLines(root: string, path: string, anchor: string, topN = 3) {
  let text: string;
  try {
    text = readFileSync(resolvePath(root, path), "utf-8");
  } catch (e) {
    return { readable: false, reason: String((e as Error).message), candidates: [], exact_hits: [] as number[] };
  }
  const head = anchorHeadLine(anchor);
  const lines = text.split("\n");
  const scored: Array<{ line: number; similarity: number; text: string }> = [];
  const exact: number[] = [];
  for (let i = 0; i < lines.length; i++) {
    const norm = normWs(lines[i]);
    if (norm.length === 0) continue;
    if (norm === head) exact.push(i + 1);
    scored.push({ line: i + 1, similarity: Math.round(diceSim(norm, head) * 1000) / 1000, text: lines[i] });
  }
  scored.sort((a, b) => b.similarity - a.similarity);
  return { readable: true, reason: "", candidates: scored.slice(0, topN), exact_hits: exact };
}

/** Turn a missed dry-run outcome into a structured, actionable diagnosis. */
function diagnose(op: Record<string, unknown>, outcome: Record<string, unknown>, opIndex: number, root: string, bin: string) {
  const kind = String(op.kind ?? "unknown");
  const path = String((op.path as string) ?? "");
  const status = String(outcome.status ?? "error");
  const anchor =
    kind === "search_replace"
      ? String((op.search as string) ?? "")
      : kind === "insert"
        ? String((op.anchor as string) ?? "")
        : "";

  const base = {
    op_index: opIndex,
    kind,
    path,
    status,
    match_tier: (outcome.match_tier as number) ?? null,
    message: (outcome.message as string) ?? null,
  };

  if (kind !== "search_replace" && kind !== "insert") {
    return { ...base, hint: "Non-anchored op failed; inspect message and re-plan." };
  }

  const probe = nearestLines(root, path, anchor, 3);
  if (!probe.readable) {
    return { ...base, anchor_head: anchorHeadLine(anchor), file_readable: false, reason: probe.reason,
      hint: "Target file could not be read at apply root — check the path is repo-relative to --root." };
  }

  if (status === "ambiguous") {
    return {
      ...base,
      anchor_head: anchorHeadLine(anchor),
      ambiguous_at_lines: probe.exact_hits,
      hint:
        "Anchor matched multiple locations. Extend `search` with more surrounding context to make it unique, " +
        "or set `line_hint` to the intended line (candidates in ambiguous_at_lines).",
    };
  }

  // anchor_miss / error: surface the closest real lines (text similarity) AND
  // structural hits (ast-grep / ctags identifier match) so the planner re-anchors.
  const struct = structuralLocate(bin, root, path, anchorHeadLine(anchor));
  return {
    ...base,
    anchor_head: anchorHeadLine(anchor),
    nearest_lines: probe.candidates,
    structural_candidates: {
      backend: struct.backend,
      identifier: struct.ident,
      hits: struct.hits,
    },
    hint:
      struct.hits.length > 0
        ? `Anchor not found. The identifier \`${struct.ident}\` really lives at ` +
          `structural_candidates.hits (${struct.backend}); re-anchor \`search\` on the exact ` +
          "line shown there, or fall back to the text-similar nearest_lines."
        : "Anchor not found. Copy `search` verbatim from one of nearest_lines (highest similarity first); " +
          "your anchor has drifted from the file's real text.",
  };
}

export default function (pi: ExtensionAPI) {
  // Goal state is owned by the selected backend. This projection exposes only
  // read and proposal surfaces to pi; it never transports a fencing token and
  // cannot commit a Goal transition directly.
  pi.registerTool({
    name: "shuttle_goal_status",
    label: "Read SHUTTLE Goal",
    description:
      "Read one local SHUTTLE Goal projection. This is observation-only: it does not " +
      "attach a controller, renew a lease, transition state, or receive a fence.",
    parameters: Type.Object({
      goal_ref: Type.String({ description: "Immutable goal:local:<store>:<id> reference." }),
      store_id: Type.Optional(Type.String({ description: "Local store identifier (default: default)." })),
      shuttle_bin: Type.Optional(Type.String({ description: "Path to the shuttle binary." })),
    }),
    async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
      const bin = eaBin(params.shuttle_bin);
      const res = spawnSync(
        bin,
        ["goal", "status", params.goal_ref, "--store-id", params.store_id ?? "default"],
        { encoding: "utf-8", maxBuffer: 8 * 1024 * 1024 },
      );
      if (res.error) return launchError(bin, res.error.message);
      if (res.status !== 0) return exitError(res.status, res.stderr);
      return {
        content: [{ type: "text", text: res.stdout }],
        details: { goal_ref: params.goal_ref, observation_only: true },
      };
    },
  });

  pi.registerTool({
    name: "shuttle_goal_propose",
    label: "Propose SHUTTLE Goal Action",
    description:
      "Write a typed, non-authoritative Goal action proposal for the SHUTTLE supervisor. " +
      "The proposal is not a committed action and contains no controller or fencing authority.",
    parameters: Type.Object({
      goal_ref: Type.String(),
      action_id: Type.String(),
      intent: Type.String(),
      payload: Type.Optional(Type.Any()),
    }),
    async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
      const proposal = {
        schema_version: "shuttle_goal_action_proposal/v1",
        goal_ref: params.goal_ref,
        action_id: params.action_id,
        intent: params.intent,
        payload: params.payload ?? {},
        authority: "none",
      };
      return {
        content: [{ type: "text", text: JSON.stringify(proposal, null, 2) }],
        details: proposal,
      };
    },
  });

  // ---------------------------------------------------------------------------
  // 0b. shuttle ui — the formal UI-proposition evidence surface (verify/couple/
  //     render/adopt/assess/canvas/receipt). Each verb is a pure function over a closed
  //     JSON contract: the `request` object is piped to `shuttle ui <verb>` on stdin
  //     and the signed JSON judgment is returned verbatim. pi never interprets
  //     the verdict; it transports evidence only. Three-valued (True/False/
  //     Null), fail-closed. render is Form A (shuttle-native), assess is Form B (an
  //     external pixel engine gated through shuttle's token hard门), and canvas is
  //     Form C (a digest-bound underlay a host renders into a Figma-class
  //     canvas). shuttle never generates pixels (red-line #6) nor becomes a second
  //     truth source (red-line #7).
  // ---------------------------------------------------------------------------
  const uiTool = (
    verb: "verify" | "couple" | "render" | "adopt" | "assess" | "canvas" | "receipt",
    label: string,
    description: string,
  ) => {
    pi.registerTool({
      name: `shuttle_ui_${verb}`,
      label,
      description,
      parameters: Type.Object({
        request: Type.Any({
          description:
            "Closed UI-proposition contract for `shuttle ui " +
            verb +
            "` (deny_unknown_fields). Sent verbatim on stdin; the signed JSON judgment is returned.",
        }),
        shuttle_bin: Type.Optional(
          Type.String({ description: "Path to the `shuttle` binary (default: SHUTTLE_BIN or `shuttle` on PATH)." }),
        ),
      }),
      async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
        const bin = eaBin(params.shuttle_bin);
        const res = spawnSync(bin, ["ui", verb], {
          input: JSON.stringify(params.request),
          encoding: "utf-8",
          maxBuffer: 32 * 1024 * 1024,
        });
        if (res.error) return launchError(bin, res.error.message);
        if (res.status !== 0) return exitError(res.status, res.stderr);
        return {
          content: [{ type: "text", text: res.stdout }],
          details: { verb, evidence_only: true },
        };
      },
    });
  };

  uiTool(
    "verify",
    "Verify UI Atoms",
    "Evaluate coord_atom/v1 geometric atoms against a rendered Frame and roll up a " +
      "three-valued verdict (satisfied/indeterminate). Geometry is a sufficient-condition " +
      "proxy: a decided true is trusted; a false on a proxy family (Order/Adjacency/Alignment/" +
      "Overlap) is a blind spot and degrades to Null, never a false. Evidence only — no authority.",
  );
  uiTool(
    "couple",
    "Couple UI Dimensions",
    "Arbitrate an L1 lexicon [value, legal_range] and L2 CSP over Value/Constraint couplings, " +
      "classifying the solver (Lint for acyclic-sparse; Solver for cyclic or dense |E|/|V|>1.5). " +
      "Accessibility dimensions are non-degradable: an unknown a11y measure blocks admissibility " +
      "(fail-closed). Returns lexicon status, solver class, a11y verdicts, and admissibility.",
  );
  uiTool(
    "render",
    "Render + Reprove UI",
    "Generate HTML from a Scene, then re-derive its geom::Frame and re-evaluate every declared " +
      "atom against the artifact it produced (render = verify loop). A decided false is a " +
      "regression; a Null is unverified, not a regression. The artifact is content-addressed " +
      "(sha256 digest) and deterministic. Optionally binds the result into a ui_projection/v1.",
  );
  uiTool(
    "adopt",
    "Adopt UI Projection",
    "Bind a verified, digest-identified artifact into a signed ui_projection/v1 whose " +
      "projection_id is derived from the artifact digest and the re-proven atom evidence. " +
      "A bad or dangling digest is rejected fail-closed. shuttle is the evidence + judgment owner; " +
      "it never usurps campaign truth and owns no physical path or land right.",
  );
  uiTool(
    "assess",
    "Assess External UI Artifact (Form B)",
    "Reverse-measure an external pixel engine's produced artifact and gate it into an " +
      "adapter_receipt/v1: a token 硬门 met by the geometry adopts, unmet & non-droppable " +
      "rejects, unmet & droppable degrades gracefully, and a blind spot (no threshold) blocks " +
      "(绝不瞎猜). a11y / capability negotiation are hard (fail-closed on unknown); the engine's " +
      "aesthetic self-report is soft (below-floor only sidecars). shuttle never generates the pixels " +
      "itself (red-line #6); the disposition is computed by the receipt fold, never laundered.",
  );
  uiTool(
    "canvas",
    "Compose UI Canvas Underlay (Form C)",
    "Overlay a Form A SVG skeleton with an optional already-adjudicated Form B adapter_receipt/v1 " +
      "into ONE digest-bound canvas_underlay/v1 (赋值 + 命题 + provenance) a host front-end renders " +
      "into a Figma-class interactive canvas. The canvas is a render product, not a truth source " +
      "(truth stays in the coord_atom propositions, red-line #7); shuttle builds neither the canvas UI " +
      "nor the pixel engine (red-lines #6 / #11) and never silently fails (§4.2) — an inadmissible " +
      "fidelity layer surfaces as blocked with no laundered node bindings.",
  );
  uiTool(
    "receipt",
    "Seal UI Phase Receipt (PDE evidence owner)",
    "Evidence-owner emit path (design §6.3): seal a verified ui_projection/v1 into a digest-bound " +
      "pde_campaign_phase_receipt/v1. Runs the fail-closed seal gate — the projection must self-verify, " +
      "and a decided effect on a non-satisfied (blind-spot) projection is refused (绝不瞎猜) — " +
      "and binds the receipt digest byte-compatibly with the pde kernel so pde can cross-verify. shuttle mirrors " +
      "the pde-campaign FROZEN schema read-only; it is the evidence/judgment owner and never writes the " +
      "campaign store nor owns any land right (red-lines #7 / #11).",
  );

  // ---------------------------------------------------------------------------
  // 1. apply_edit_plan — the write path, fronted by a dry-run gate.
  // ---------------------------------------------------------------------------
  pi.registerTool({
    name: "apply_edit_plan",
    label: "Apply EditPlan",
    description:
      "Apply an EditPlan (content-anchored search/replace / insert / create / codemod ops) " +
      "via the shuttle `shuttle` executor. This is the general write path — pi's " +
      "built-in edit/write are disabled. LADDER — pick the operator that KNOWS the most: " +
      "(1) FIRST, if the change is a named structural operation (rename a symbol, extract/inline " +
      "a variable/constant/method, change/introduce/inline/remove a parameter, move a function, " +
      "extract to a new file, implement interface members, generate boilerplate/records, scaffold " +
      "a test, create-from-usage), use `apply_refactor` — it derives the edit from the AST (params, " +
      "fields, call sites) and returns a machine-readable confidence (high=safe to apply; " +
      "medium=review the diff; low=blocked unless force) + warnings; a §7 ambiguous/unknowable " +
      "target is REJECTED, never guessed. (2) Fall back to THIS tool only for edits no operator " +
      "covers — free-form insertions, config/text tweaks, or hand-anchored patches. Behavior: a " +
      "deterministic dry-run gate first probes every op WITHOUT writing; only if all ops resolve " +
      "does it commit the real write. If an op's anchor misses or is ambiguous, NOTHING is written " +
      "and a structured diagnosis (failed op index, status, nearest real lines / ambiguous line " +
      "numbers, and a fix hint) is returned so you correct the plan in one round. Never rewrite " +
      "whole files; emit minimal content-anchored ops. Pass dry_run:true to force preview-only.",
    parameters: Type.Object({
      plan: Type.Object(
        {
          version: Type.Number({ description: "Schema version; must be 1." }),
          intent: Type.Optional(
            Type.String({ description: "Human/agent-readable intent, for audit." }),
          ),
          ops: Type.Array(Type.Any(), {
            description:
              "List of EditOps. Each op is {kind, ...}: search_replace {path, search, replace, line_hint?}, " +
              "insert {path, anchor, position, content}, create_file {path, content}, or codemod {rule_id, paths?}.",
          }),
        },
        { description: "The EditPlan to apply." },
      ),
      root: Type.Optional(
        Type.String({ description: "Repo root for codemod/locate scope (default: cwd)." }),
      ),
      dry_run: Type.Optional(
        Type.Boolean({ description: "Preview only; do not write to disk and skip the auto-commit gate (default false)." }),
      ),
      shuttle_bin: Type.Optional(
        Type.String({ description: "Path to the `shuttle` binary (default: SHUTTLE_BIN or `shuttle` on PATH)." }),
      ),
    }),
    async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
      const bin = eaBin(params.shuttle_bin);
      const root = process.env.SHUTTLE_ENFORCE_SCOPE === "1"
        ? (process.env.SHUTTLE_WORKSPACE_ROOT ?? ".")
        : (params.root ?? ".");

      // Local helper: spawn one `shuttle apply` pass with the given extra flags
      // (e.g. ["--dry-run"] for a preview, ["--gate"] for the single-pass
      // probe+commit) and hand back the raw spawn result.
      const runApply = (flags: string[]) => {
        const args = ["apply", "--root", root, ...flags];
        // Fail-closed scope enforcement handed down by `shuttle cell-run` via the
        // child env (the extension never sees the CellBinding directly). When
        // set, every apply pass is confined to the binding's allowed_paths —
        // closing the resolver=None escape a bare `shuttle run` leaves open.
        if (process.env.SHUTTLE_ENFORCE_SCOPE === "1") {
          args.push("--enforce-scope");
          const allowed = process.env.SHUTTLE_ALLOWED_PATHS ?? "";
          for (const p of allowed.split(",").map((x) => x.trim()).filter(Boolean)) {
            args.push("--allowed-path", p);
          }
        }
        return spawnSync(bin, args, {
          input: JSON.stringify(params.plan),
          encoding: "utf-8",
          maxBuffer: 32 * 1024 * 1024,
        });
      };

      const parseOutcomes = (stdout: string): Array<Record<string, unknown>> => {
        try {
          const o = JSON.parse(stdout);
          return Array.isArray(o) ? (o as Array<Record<string, unknown>>) : [];
        } catch {
          return [];
        }
      };

      // Escape hatch: explicit dry_run keeps the original preview-only behavior.
      if (params.dry_run) {
        const res = runApply(["--dry-run"]);
        if (res.error) return launchError(bin, res.error.message);
        if (res.status !== 0) return exitError(res.status, res.stderr);
        const outcomes = parseOutcomes(res.stdout);
        const applied = outcomes.filter((o) => o.status === "applied").length;
        const missed = outcomes.length - applied;
        recordTelemetry({ tool: "apply_edit_plan", opCount: outcomes.length, outcome: `dry_run:${applied}/${outcomes.length}`, wrote: false });
        return {
          content: [
            { type: "text", text: `Dry-run: ${applied}/${outcomes.length} op(s) would apply` + (missed > 0 ? `, ${missed} would miss.` : ".") },
            { type: "text", text: JSON.stringify(outcomes, null, 2) },
          ],
          details: { dry_run: true, applied, missed, outcomes },
        };
      }

      // ---- Single-pass gate: probe + commit in ONE `shuttle apply --gate`. ----
      // The core runs the dry-run probe in-process and writes ONLY if every op
      // resolves (all-or-nothing, aligned with P0-2 `Rejected` fail-closed).
      // One spawn, plan parsed once — kills W-C's second cold start + re-parse.
      // Contract with the core: exit 0 + any non-"applied" outcome means the
      // probe blocked and NOTHING was written (the write pass only runs when the
      // probe is all-green, and re-evaluating the same resolved ops on unchanged
      // files is deterministic); a real IO failure surfaces as a non-zero exit.
      const res = runApply(["--gate"]);
      if (res.error) return launchError(bin, res.error.message);
      if (res.status !== 0) return exitError(res.status, res.stderr);
      const outcomes = parseOutcomes(res.stdout);

      const ops = Array.isArray((params.plan as { ops?: unknown }).ops)
        ? ((params.plan as { ops: Array<Record<string, unknown>> }).ops)
        : [];
      const failedIdx = outcomes
        .map((o, i) => ({ o, i }))
        .filter(({ o }) => o.status !== "applied");

      // ---- Gate blocked: nothing was written; return structured diagnosis. ----
      if (failedIdx.length > 0) {
        const diagnostics = failedIdx.map(({ o, i }) => diagnose(ops[i] ?? {}, o, i, root, bin));
        const summary =
          `Gate blocked the write: ${failedIdx.length}/${outcomes.length} op(s) failed the dry-run ` +
          `probe (no files were modified). Fix the anchors below and resubmit — each diagnosis lists the ` +
          `nearest real lines or the ambiguous line numbers.`;
        recordTelemetry({ tool: "apply_edit_plan", opCount: outcomes.length, outcome: "gate_blocked", wrote: false, error: true });
        return {
          content: [
            { type: "text", text: summary },
            { type: "text", text: JSON.stringify({ gate: "blocked", diagnostics }, null, 2) },
          ],
          details: { gate: "blocked", applied: 0, missed: failedIdx.length, diagnostics, probe: outcomes },
          isError: true,
        };
      }

      // ---- Gate passed: the core already committed every op in the same pass. ----
      const applied = outcomes.filter((o) => o.status === "applied").length;
      recordTelemetry({ tool: "apply_edit_plan", opCount: outcomes.length, outcome: "gate_passed", wrote: true });
      return {
        content: [
          { type: "text", text: `Gate passed: validated then applied ${applied}/${outcomes.length} op(s) in one pass.` },
          { type: "text", text: JSON.stringify(outcomes, null, 2) },
        ],
        details: { gate: "passed", applied, missed: 0, outcomes },
      };
    },
  });

  // ---------------------------------------------------------------------------
  // 1b. apply_refactor — the deterministic REFACTOR ladder (§9). Prefer this
  //     over hand-planning search_replace ops whenever the change matches a
  //     named operation below: each op derives its edit from the AST (params,
  //     fields, call sites) and comes back with a machine-readable `confidence`
  //     (high / medium / low) + `warnings`, so you can decide whether to pause.
  //     Flow: we always run a `--json` preview first (no disk write) to surface
  //     confidence/warnings; then, unless preview_only, we write — but a
  //     Low-confidence plan is NOT written without force:true (§9.5 gate).
  //     A Rejected op (ambiguous target, guess-would-be-needed, §7) surfaces as
  //     a structured error, never a silent bad edit.
  // ---------------------------------------------------------------------------
  // Per-op flag spec: which `args` keys map to which `--flag`, and which are
  // required. `file` is the positional. This is the single source of truth the
  // argv builder and the required-arg validator both read — no op is guessed.
  const REFACTOR_OPS: Record<
    string,
    { flags: Record<string, string>; required: string[]; bools?: string[] }
  > = {
    "rename-local": { flags: { line: "--line", col: "--col", to: "--to" }, required: ["line", "col", "to"] },
    "rename-symbol": { flags: { name: "--name", to: "--to", root: "--root" }, required: ["name", "to"] },
    "inline-variable": { flags: { line: "--line", col: "--col" }, required: ["line", "col"] },
    "extract-variable": { flags: { line: "--line", start_col: "--start-col", end_col: "--end-col", name: "--name" }, required: ["line", "start_col", "end_col", "name"] },
    "extract-constant": { flags: { line: "--line", start_col: "--start-col", end_col: "--end-col", name: "--name" }, required: ["line", "start_col", "end_col", "name"] },
    "inline-constant": { flags: { line: "--line", col: "--col" }, required: ["line", "col"] },
    "extract-method": { flags: { start_line: "--start-line", end_line: "--end-line", name: "--name" }, required: ["start_line", "end_line", "name"] },
    "inline-method": { flags: { line: "--line", col: "--col" }, required: ["line", "col"] },
    "change-signature": { flags: { func: "--func", params: "--params", root: "--root" }, required: ["func", "params"] },
    "introduce-parameter": { flags: { func: "--func", expr: "--expr", name: "--name", root: "--root" }, required: ["func", "expr", "name"] },
    "inline-parameter": { flags: { func: "--func", name: "--name", root: "--root" }, required: ["func", "name"] },
    "remove-parameter": { flags: { func: "--func", name: "--name", root: "--root" }, required: ["func", "name"] },
    "move-function": { flags: { func: "--func", to: "--to", root: "--root" }, required: ["func", "to"], bools: ["add-import", "rewrite-callers", "clean-dest-import"] },
    "extract-to-new-file": { flags: { name: "--name", to: "--to", root: "--root" }, required: ["name", "to"], bools: ["add-import", "rewrite-callers"] },
    "implement-members": { flags: { interface: "--interface", as: "--as" }, required: ["interface", "as"] },
    "generate-boilerplate": { flags: { type: "--type", what: "--what" }, required: ["type"] },
    "create-from-usage": { flags: { name: "--name" }, required: ["name"] },
    "scaffold-test": { flags: { name: "--name" }, required: ["name"] },
    "record-projection": { flags: { name: "--name" }, required: ["name"] },
  };

  pi.registerTool({
    name: "apply_refactor",
    label: "Apply Refactor",
    description:
      "Run one of shuttle's DETERMINISTIC refactor operations — the AST-derived edit ladder (§9). " +
      "PREFER this over hand-planning search_replace/insert ops in apply_edit_plan whenever the " +
      "change is a named operation: the edit is projected from the syntax tree (parameters, " +
      "fields, call sites) and returns a machine-readable confidence (high/medium/low) + warnings. " +
      "We always preview with --json first (no write); then write unless preview_only:true, but a " +
      "Low-confidence plan is blocked unless force:true. A Rejected op (ambiguous/unknowable target) " +
      "comes back as a structured error — nothing is written. Ops & their `args` keys: " +
      "rename-local{line,col,to}, rename-symbol{name,to,root?}, inline-variable{line,col}, " +
      "extract-variable{line,start_col,end_col,name}, extract-constant{line,start_col,end_col,name}, " +
      "inline-constant{line,col}, extract-method{start_line,end_line,name}, inline-method{line,col}, " +
      "change-signature{func,params,root?}, introduce-parameter{func,expr,name,root?}, " +
      "inline-parameter{func,name,root?}, remove-parameter{func,name,root?}, " +
      "move-function{func,to,root?,'add-import'?,'rewrite-callers'?,'clean-dest-import'?}, " +
      "extract-to-new-file{name,to,root?,'add-import'?,'rewrite-callers'?}, " +
      "implement-members{interface,as}, generate-boilerplate{type,what?}, create-from-usage{name}, " +
      "scaffold-test{name}, record-projection{name}. `args` values are strings; boolean opts are 'true'.",
    parameters: Type.Object({
      op: Type.Union(
        Object.keys(REFACTOR_OPS).map((k) => Type.Literal(k)),
        { description: "The refactor operation to run (see description for each op's args)." },
      ),
      file: Type.String({ description: "The file the refactor operates on (positional <FILE>)." }),
      args: Type.Optional(
        Type.Record(Type.String(), Type.String(), {
          description:
            "Op-specific arguments keyed by the names in the op list, e.g. {func:'foo',params:'keep:1,keep:0'}. " +
            "Boolean opt-in flags (add-import, rewrite-callers, clean-dest-import) are passed as 'true'.",
        }),
      ),
      preview_only: Type.Optional(
        Type.Boolean({ description: "Only preview the plan (--json, no write). Default false (preview then write)." }),
      ),
      force: Type.Optional(
        Type.Boolean({ description: "Write even a Low-confidence plan (§9.5 gate override). Default false." }),
      ),
      shuttle_bin: Type.Optional(Type.String({ description: "Path to the `shuttle` binary." })),
    }),
    async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
      const bin = eaBin(params.shuttle_bin);
      const spec = REFACTOR_OPS[params.op];
      if (!spec) {
        return { content: [{ type: "text", text: `Unknown refactor op '${params.op}'.` }], details: { op: params.op }, isError: true };
      }
      const args = params.args ?? {};

      // Required-arg validation up front — fail with a precise message rather
      // than shelling out to a clap error the agent has to reverse-engineer.
      const missing = spec.required.filter((k) => args[k] === undefined || args[k] === "");
      if (missing.length > 0) {
        return {
          content: [{ type: "text", text: `Missing required arg(s) for ${params.op}: ${missing.join(", ")}. Expected keys: ${spec.required.join(", ")}.` }],
          details: { op: params.op, missing },
          isError: true,
        };
      }

      // Build the argv faithfully from the spec (no guessing which flag is which).
      const build = (extra: string[]): string[] => {
        const argv = ["refactor", params.op, params.file];
        for (const [key, flag] of Object.entries(spec.flags)) {
          const v = args[key];
          if (v !== undefined && v !== "") argv.push(flag, v);
        }
        for (const b of spec.bools ?? []) {
          if (args[b] === "true") argv.push(`--${b}`);
        }
        argv.push(...extra);
        return argv;
      };

      const runEa = (extra: string[]) =>
        spawnSync(bin, build(extra), { encoding: "utf-8", maxBuffer: 32 * 1024 * 1024 });

      // ---- Phase 1: --json preview. Rejected ops exit non-zero here. ----
      const pv = runEa(["--json"]);
      if (pv.error) return launchError(bin, pv.error.message);
      if (pv.status !== 0) {
        // A §7 Rejected (ambiguous / unknowable) surfaces as a non-zero exit
        // with the reason on stderr — hand it back structured, write nothing.
        return {
          content: [{ type: "text", text: `Refactor '${params.op}' was rejected (no edit produced):\n${(pv.stderr || "").trim()}` }],
          details: { op: params.op, rejected: true, stderr: pv.stderr },
          isError: true,
        };
      }
      let plan: Record<string, unknown> = {};
      try { plan = JSON.parse(pv.stdout); } catch { plan = {}; }
      const confidence = String(plan.confidence ?? "unknown");
      const warnings = Array.isArray(plan.warnings) ? (plan.warnings as string[]) : [];
      const sites = Array.isArray(plan.sites) ? (plan.sites as string[]) : [];
      const opCount = Array.isArray((plan.plan as { ops?: unknown[] })?.ops)
        ? ((plan.plan as { ops: unknown[] }).ops.length)
        : 0;

      // ---- Preview-only, or a low-confidence plan without force: stop here. ----
      const lowBlocked = confidence === "low" && !params.force && !params.preview_only;
      if (params.preview_only || lowBlocked) {
        const why = lowBlocked
          ? `Low-confidence plan — NOT written. Review the warnings; resubmit with force:true to apply.`
          : `Preview only — no files were modified.`;
        recordTelemetry({ tool: "apply_refactor", op: params.op, opCount, outcome: lowBlocked ? "low_blocked" : "preview_only", confidence, warnings: warnings.length, wrote: false });
        return {
          content: [
            { type: "text", text: `Refactor '${params.op}': ${confidence} confidence, ${opCount} edit(s). ${why}` },
            { type: "text", text: JSON.stringify({ confidence, warnings, sites }, null, 2) },
          ],
          details: { op: params.op, wrote: false, confidence, warnings, sites, blocked: lowBlocked, plan },
        };
      }

      // ---- Phase 2: commit. --write (+ --force if the caller allowed it). ----
      const wr = runEa(params.force ? ["--write", "--force", "--json"] : ["--write", "--json"]);
      if (wr.error) return launchError(bin, wr.error.message);
      if (wr.status !== 0) return exitError(wr.status, wr.stderr);
      let outcomes: Array<Record<string, unknown>> = [];
      try { const o = JSON.parse(wr.stdout); if (Array.isArray(o)) outcomes = o; } catch { /* human line on stderr */ }
      const applied = outcomes.filter((o) => o.status === "applied").length;
      recordTelemetry({ tool: "apply_refactor", op: params.op, opCount: applied || opCount, outcome: "applied", confidence, warnings: warnings.length, wrote: true });
      return {
        content: [
          { type: "text", text: `Refactor '${params.op}' applied (${confidence} confidence): ${applied || opCount} edit(s) written.` + (warnings.length ? ` ${warnings.length} warning(s) — see details.` : "") },
          { type: "text", text: JSON.stringify({ confidence, warnings, sites, outcomes }, null, 2) },
        ],
        details: { op: params.op, wrote: true, confidence, warnings, sites, outcomes },
      };
    },
  });

  // ---------------------------------------------------------------------------
  // 2. shuttle_context_pack — assemble the upstream contract a leaf task needs.
  //    Given the split `tasks/` dir + this task id (or an explicit
  //    depends_on / touch_hint), it (a) resolves DIRECT predecessors, (b) takes
  //    the predecessors' touch_hint ∩/adjacent this task's touch_hint = the
  //    handoff files, and (c) emits a token-budgeted symbol skeleton for those
  //    files via `shuttle repomap --focus` + `shuttle locate`. When metadata is missing it
  //    falls back to `git diff --name-only` (files a predecessor actually
  //    changed) and finally to a plain repomap. Pure shuttle binary + git — no model.
  // ---------------------------------------------------------------------------
  pi.registerTool({
    name: "shuttle_context_pack",
    label: "Pack upstream context",
    description:
      "Assemble the minimal upstream contract THIS leaf subtask needs before it edits code. " +
      "Point it at the split `tasks_dir` and `task_id`: it reads depends_on to find direct " +
      "predecessor subtasks, intersects their touch_hint with this task's touch_hint to find the " +
      "handoff files, and returns a token-budgeted symbol skeleton (signatures of what upstream " +
      "just produced) via shuttle repomap/locate — so you build on the real interface instead of " +
      "guessing or re-reading whole files. If the tasks lack structured metadata, pass " +
      "`git_base` to infer changed files from `git diff --name-only`, or omit everything for a " +
      "plain focused repomap. Zero model tokens; all work runs through the shuttle binary and git.",
    parameters: Type.Object({
      tasks_dir: Type.Optional(
        Type.String({ description: "Path to the split `tasks/` dir holding task-*.yaml (for metadata-driven packing)." }),
      ),
      task_id: Type.Optional(
        Type.String({ description: "The current subtask id (e.g. task-31); its depends_on + touch_hint drive the pack." }),
      ),
      depends_on: Type.Optional(
        Type.Array(Type.String(), { description: "Explicit predecessor ids, overrides the yaml depends_on." }),
      ),
      touch_hint: Type.Optional(
        Type.Array(Type.String(), { description: "Explicit files this task touches, overrides the yaml touch_hint." }),
      ),
      git_base: Type.Optional(
        Type.String({ description: "Git ref/range for the no-metadata fallback: files from `git diff --name-only <git_base>` become the focus set." }),
      ),
      symbol_index: Type.Optional(
        Type.String({ description: "Caller-owned pure symbol-index artifact for verified symbol truth." }),
      ),
      index_head: Type.Optional(
        Type.String({ description: "Revision the supplied symbol index must exactly represent (default HEAD)." }),
      ),
      write_symbols: Type.Optional(
        Type.Array(Type.String(), { description: "Legacy planner declarations in NAME@REPO_PATH form. The adapter never treats these delimiters as exact; only Rust-resolved canonical symbol_footprint output is typed truth." }),
      ),
      budget: Type.Optional(
        Type.Number({ description: "Token budget for the emitted symbol skeleton (default 1024)." }),
      ),
      root: Type.Optional(Type.String({ description: "Repo root (default: cwd)." })),
      shuttle_bin: Type.Optional(Type.String({ description: "Path to the `shuttle` binary." })),
    }),
    async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
      const bin = eaBin(params.shuttle_bin);
      const root = params.root ?? ".";
      const budget = params.budget ?? 1024;

      return withRunCache(
        root,
        "shuttle_context_pack",
        {
          tasks_dir: params.tasks_dir ?? null,
          task_id: params.task_id ?? null,
          depends_on: params.depends_on ?? null,
          touch_hint: params.touch_hint ?? null,
          git_base: params.git_base ?? null,
          symbol_index: params.symbol_index ?? null,
          index_head: params.index_head ?? null,
          write_symbols: params.write_symbols ?? null,
          budget,
        },
        () => {
      // ONE spawn: the Rust `shuttle context-pack` subcommand does the whole
      // resolve depends_on -> intersect touch_hint -> repomap orchestration
      // in-process, reusing a single SymbolTable (kills W-B: 10 spawns -> 1).
      const args = ["context-pack", "--root", root, "--budget", String(budget)];
      if (params.tasks_dir) args.push("--tasks-dir", params.tasks_dir);
      if (params.task_id) args.push("--task-id", params.task_id);
      if (params.depends_on && params.depends_on.length > 0) args.push("--depends-on", params.depends_on.join(","));
      if (params.touch_hint && params.touch_hint.length > 0) args.push("--touch-hint", params.touch_hint.join(","));
      if (params.git_base) args.push("--git-base", params.git_base);
      if (params.symbol_index) args.push("--symbol-index", params.symbol_index);
      if (params.index_head) args.push("--index-head", params.index_head);
      if (params.write_symbols && params.write_symbols.length > 0) args.push("--write-symbol", params.write_symbols.join(","));

      const res = spawnSync(bin, args, { encoding: "utf-8", maxBuffer: 32 * 1024 * 1024 });
      if (res.error) return launchError(bin, res.error.message);
      if (res.status !== 0) return exitError(res.status, res.stderr);

      let pack: Record<string, any>;
      try {
        const parsed: unknown = JSON.parse(res.stdout);
        if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error("context-pack stdout must be a JSON object");
        pack = parsed as Record<string, any>;
        if (pack.symbol_footprint !== undefined) validateSymbolFootprint(pack.symbol_footprint);
      } catch (error) {
        return adapterError(`invalid context-pack typed output: ${error instanceof Error ? error.message : String(error)}`);
      }

      const summary = String(pack.summary ?? `Context pack for ${params.task_id ?? "(ad-hoc)"}.`);
      const predecessors = Array.isArray(pack.predecessors) ? pack.predecessors : [];
      const sharedHandoff = Array.isArray(pack.shared_handoff_files) ? pack.shared_handoff_files : [];
      const upstreamOnly = Array.isArray(pack.upstream_only_files) ? pack.upstream_only_files : [];
      const focus = Array.isArray(pack.focus) ? pack.focus : [];
      const contract = Array.isArray(pack.contract_symbols) ? pack.contract_symbols : [];
      const readSymbols = Array.isArray(pack.read_symbols) ? pack.read_symbols : [];
      const writeSymbols = Array.isArray(pack.write_symbols) ? pack.write_symbols : [];
      const handoffSymbols = Array.isArray(pack.handoff_symbols) ? pack.handoff_symbols : [];

      return {
        content: [
          { type: "text", text: summary },
          {
            type: "text",
            text: JSON.stringify(
              {
                task: pack.task ?? null,
                source: pack.source ?? null,
                predecessors,
                shared_handoff_files: sharedHandoff,
                upstream_only_files: upstreamOnly,
                focus,
                contract_symbols: contract,
                ...(pack.index_status ? { index_status: pack.index_status } : {}),
                ...(readSymbols.length ? { read_symbols: readSymbols } : {}),
                ...(writeSymbols.length ? { write_symbols: writeSymbols } : {}),
                ...(handoffSymbols.length ? { handoff_symbols: handoffSymbols } : {}),
                ...(pack.symbol_footprint ? { symbol_footprint: pack.symbol_footprint } : {}),
              },
              null,
              2,
            ),
          },
        ],
        details: { source: pack.source ?? null, index_status: pack.index_status ?? null, predecessors, focus, shared: sharedHandoff, upstream_only: upstreamOnly, contract_count: contract.length, read_symbols: readSymbols, write_symbols: writeSymbols, handoff_symbols: handoffSymbols, symbol_footprint: pack.symbol_footprint ?? null, legacy_declaration_source: params.write_symbols?.length ? "planner_guess" : null, typed_symbol_evidence: pack.symbol_footprint !== undefined, grants_path_authority: false },
      };
        },
      );
    },
  });

  // ---------------------------------------------------------------------------
  // 3. shuttle_goal_parallel_advice — read-only closed v1/v2 advice bridge.
  // ---------------------------------------------------------------------------
  pi.registerTool({
    name: "shuttle_goal_parallel_advice",
    label: "Produce SHUTTLE parallel advice",
    description:
      "Produce read-only SHUTTLE parallel advice from a plan. v2 requires a closed canonical " +
      "shuttle_symbol_footprint/v1 object and returns the closed shuttle_goal_parallel_advice/v2 object; " +
      "v1 is explicit compatibility and remains planner_guess. This tool grants no path, " +
      "workspace, dispatch, land, push, claim or approval authority.",
    parameters: Type.Object({
      plan: Type.String({ description: "Path to the intent JSON plan." }),
      symbol_footprint: Type.Optional(Type.Any({ description: "Closed canonical shuttle_symbol_footprint/v1 object. Selects v2 under auto; required for explicit v2 and forbidden for explicit v1." })),
      advice_version: Type.Optional(Type.Union([Type.Literal("auto"), Type.Literal("v1"), Type.Literal("v2")], { description: "Advice wire version. Default auto selects v2 with symbol_footprint, otherwise v1." })),
      root: Type.Optional(Type.String({ description: "Repo root (default cwd)." })),
      shuttle_bin: Type.Optional(Type.String({ description: "Path to the shuttle binary." })),
    }, { additionalProperties: false }),
    async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
      const bin = eaBin(params.shuttle_bin);
      const root = params.root ?? ".";
      const requestedVersion = params.advice_version ?? "auto";
      if (requestedVersion === "v2" && params.symbol_footprint === undefined) return adapterError("v2 requires canonical symbol_footprint object");
      if (requestedVersion === "v1" && params.symbol_footprint !== undefined) return adapterError("v1 does not accept symbol_footprint; it remains planner_guess");
      const selectedVersion = requestedVersion === "auto"
        ? (params.symbol_footprint === undefined ? "v1" : "v2")
        : requestedVersion;
      let footprintDigest: string | undefined;
      try {
        if (params.symbol_footprint !== undefined) {
          validateSymbolFootprint(params.symbol_footprint);
          footprintDigest = canonicalDigest(params.symbol_footprint);
        }
      } catch (error) {
        return adapterError(`invalid symbol_footprint: ${error instanceof Error ? error.message : String(error)}`);
      }

      const temp = mkdtempSync(join(tmpdir(), "shuttle-advice-adapter-"));
      try {
        const advicePath = join(temp, "parallel-advice.json");
        const args = ["goal", "--plan", params.plan, "--root", root, "--dry-run", "--parallel", "--advice-out", advicePath, "--json"];
        if (selectedVersion === "v2") {
          const footprintPath = join(temp, "symbol-footprint.json");
          writeFileSync(footprintPath, `${canonicalJson(params.symbol_footprint)}\n`, { encoding: "utf-8", mode: 0o600 });
          if (requestedVersion === "v2") args.push("--advice-version", "v2");
          args.push("--symbol-footprint", footprintPath);
        } else if (requestedVersion === "v1") {
          args.push("--advice-version", "v1");
        }
        const res = spawnSync(bin, args, { encoding: "utf-8", maxBuffer: 32 * 1024 * 1024 });
        if (res.error) return launchError(bin, res.error.message);
        if (res.status !== 0) return exitError(res.status, res.stderr);
        let advice: Record<string, any>;
        try {
          advice = JSON.parse(readFileSync(advicePath, "utf-8"));
          if (selectedVersion === "v2") validateHybridAdvice(advice, footprintDigest); else validateV1Advice(advice);
        } catch (error) {
          return adapterError(`invalid parallel advice output: ${error instanceof Error ? error.message : String(error)}`);
        }
        return {
          content: [{ type: "text" as const, text: JSON.stringify(advice, null, 2) }],
          details: { advice, advice_digest: canonicalDigest(advice), read_only: true, authority: "none", selected_advice_version: selectedVersion },
        };
      } finally {
        rmSync(temp, { recursive: true, force: true });
      }
    },
  });

  // ---------------------------------------------------------------------------
  // 4. shuttle_locate — find anchors before planning search/replace.
  // ---------------------------------------------------------------------------
  pi.registerTool({
    name: "shuttle_locate",
    label: "Locate anchors",
    description:
      "Find where a pattern occurs in the repo BEFORE planning a search_replace op. " +
      "With `lang` set it does a structural (ast-grep) search; without it, a fast text (ripgrep) " +
      "search. Returns hits as `path:line:col: text`. Use this to pick a unique anchor so your " +
      "edit lands on the first fuzzy tier instead of missing.",
    parameters: Type.Object({
      pattern: Type.String({ description: "Text substring or ast-grep pattern to locate." }),
      lang: Type.Optional(
        Type.String({ description: "Language for structural search (e.g. js, py, rust). Omit for text search." }),
      ),
      root: Type.Optional(Type.String({ description: "Repo root to search (default: cwd)." })),
      shuttle_bin: Type.Optional(Type.String({ description: "Path to the `shuttle` binary." })),
    }),
    async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
      const bin = eaBin(params.shuttle_bin);
      const args = ["locate", "--pattern", params.pattern, "--root", params.root ?? "."];
      if (params.lang) args.push("--lang", params.lang);

      const res = spawnSync(bin, args, { encoding: "utf-8", maxBuffer: 32 * 1024 * 1024 });
      if (res.error) return launchError(bin, res.error.message);
      if (res.status !== 0) return exitError(res.status, res.stderr);

      const lines = res.stdout.split("\n").filter((l) => l.trim().length > 0);
      const summary =
        lines.length === 0
          ? `No hits for pattern ${JSON.stringify(params.pattern)}.`
          : `${lines.length} hit(s) for ${JSON.stringify(params.pattern)}.`;
      return {
        content: [
          { type: "text", text: summary },
          { type: "text", text: res.stdout || "(no output)" },
        ],
        details: { hits: lines.length },
      };
    },
  });

  // ---------------------------------------------------------------------------
  // 4. shuttle_repomap — token-budgeted repo map for planning context.
  // ---------------------------------------------------------------------------
  pi.registerTool({
    name: "shuttle_repomap",
    label: "Repo map",
    description:
      "Emit a token-budgeted repo map (top symbols/files ranked by a reference-graph PageRank, " +
      "§7.5) to orient before editing an unfamiliar codebase. Pass `focus` files the task touches " +
      "for personalized weighting. Cheaper and more targeted than reading files one by one.",
    parameters: Type.Object({
      budget: Type.Optional(
        Type.Number({ description: "Token budget for the emitted map (default 1024)." }),
      ),
      focus: Type.Optional(
        Type.Array(Type.String(), {
          description: "Files the current task touches (personalized-PageRank weighting).",
        }),
      ),
      root: Type.Optional(Type.String({ description: "Repo root (default: cwd)." })),
      shuttle_bin: Type.Optional(Type.String({ description: "Path to the `shuttle` binary." })),
    }),
    async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
      const bin = eaBin(params.shuttle_bin);
      const root = params.root ?? ".";
      return withRunCache(
        root,
        "shuttle_repomap",
        { budget: params.budget ?? 1024, focus: params.focus ?? [] },
        () => {
          const args = ["repomap", "--root", root];
          if (params.budget != null) args.push("--budget", String(params.budget));
          for (const f of params.focus ?? []) args.push("--focus", f);

          const res = spawnSync(bin, args, { encoding: "utf-8", maxBuffer: 32 * 1024 * 1024 });
          if (res.error) return launchError(bin, res.error.message);
          if (res.status !== 0) return exitError(res.status, res.stderr);

          return {
            content: [{ type: "text", text: res.stdout || "{}" }],
            details: { budget: params.budget ?? 1024 },
          };
        },
      );
    },
  });

  // ---------------------------------------------------------------------------
  // 5. shuttle_rename — symbol-table structural rename (zero model tokens).
  // ---------------------------------------------------------------------------
  pi.registerTool({
    name: "shuttle_rename",
    label: "Rename symbol",
    description:
      "Rename a symbol across the repo via the symbol table (ctags + tree-sitter scope " +
      "disambiguation). This is a structural, zero-token edit — prefer it over hand-planning " +
      "many search_replace ops for a rename. Set dry_run to preview the edits first.",
    parameters: Type.Object({
      from: Type.String({ description: "Current symbol name." }),
      to: Type.String({ description: "New symbol name." }),
      root: Type.Optional(Type.String({ description: "Repo root (default: cwd)." })),
      dry_run: Type.Optional(
        Type.Boolean({ description: "Preview edits only; do not write (default false)." }),
      ),
      shuttle_bin: Type.Optional(Type.String({ description: "Path to the `shuttle` binary." })),
    }),
    async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
      const bin = eaBin(params.shuttle_bin);
      const root = params.root ?? ".";
      const run = () => {
        const args = ["rename", "--from", params.from, "--to", params.to, "--root", root];
        if (params.dry_run) args.push("--dry-run");

        const res = spawnSync(bin, args, { encoding: "utf-8", maxBuffer: 32 * 1024 * 1024 });
        if (res.error) return launchError(bin, res.error.message);
        if (res.status !== 0) return exitError(res.status, res.stderr);

        const verb = params.dry_run ? "Previewed" : "Applied";
        return {
          content: [
            { type: "text", text: `${verb} rename ${params.from} → ${params.to}.` },
            { type: "text", text: res.stdout || "[]" },
          ],
          details: { from: params.from, to: params.to, dry_run: params.dry_run ?? false },
        };
      };
      // Only dry-run previews are cache-safe; a real rename writes to disk and
      // must always run.
      if (!params.dry_run) return run();
      return withRunCache(root, "shuttle_rename", { from: params.from, to: params.to, dry_run: true }, run);
    },
  });

  // ---------------------------------------------------------------------------
  // 6. conflict_propose — L2 semantic-merge submission (see W7 / docs §8.5).
  //    The ONLY conflict tool surfaced to pi; perception (list/get-hunk/
  //    classify), deterministic merge (auto3way/take), verify and stage are
  //    all handled by the shuttle supervisor as `shuttle conflict *` subcommands. Use
  //    ONLY for L2 semantic hunks that auto3way could not resolve.
  // ---------------------------------------------------------------------------
  pi.registerTool({
    name: "conflict_propose",
    label: "Propose Conflict Merge",
    description:
      "Submit an LLM-arbitrated merge resolution for ONE git conflict hunk. " +
      "This is the ONLY conflict tool available to the planner; perception " +
      "(list/get-hunk/classify), deterministic merge (auto3way/take), verify " +
      "and stage are all handled by the shuttle supervisor as `shuttle conflict *` " +
      "subcommands. Use ONLY for L2 semantic hunks that auto3way could not " +
      "resolve. merged_text MUST NOT contain any conflict markers " +
      "(<<<<<<<, =======, >>>>>>>) and must preserve BOTH sides' intent. " +
      "After this call, shuttle runs verify + stage automatically.",
    parameters: Type.Object({
      file: Type.String({
        description:
          "Repo-relative path of the conflicted file (must match `shuttle conflict list`).",
      }),
      hunk_id: Type.Number({
        description:
          "Zero-based hunk index within the file (must match `shuttle conflict get-hunk`).",
      }),
      merged_text: Type.String({
        description:
          "Resolved text replacing the entire conflict hunk region. No conflict markers.",
      }),
      rationale: Type.Optional(
        Type.String({
          description: "One-sentence justification, recorded for the audit trail.",
        }),
      ),
      root: Type.Optional(
        Type.String({ description: "Repo root (default: cwd)." }),
      ),
      shuttle_bin: Type.Optional(
        Type.String({ description: "Path to the `shuttle` binary (default: SHUTTLE_BIN or `shuttle` on PATH)." }),
      ),
    }),
    async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
      // Guard client-side too: never let a marker-laden proposal through.
      if (/^(<<<<<<<|=======|>>>>>>>)/m.test(params.merged_text)) {
        return {
          content: [
            {
              type: "text",
              text: "merged_text still contains conflict markers; resolve them before proposing.",
            },
          ],
          details: { rejected: "conflict_markers_present" },
          isError: true,
        };
      }

      const bin = eaBin(params.shuttle_bin);
      // `shuttle conflict propose <file> <hunk_id> --text <text> [--root .]`
      // positional file + hunk_id; --text carries the merged region.
      const args = [
        "conflict",
        "propose",
        params.file,
        String(params.hunk_id),
        "--text",
        params.merged_text,
        "--root",
        params.root ?? ".",
      ];

      const res = spawnSync(bin, args, { encoding: "utf-8", maxBuffer: 32 * 1024 * 1024 });
      if (res.error) return launchError(bin, res.error.message);
      if (res.status !== 0) return exitError(res.status, res.stderr);

      return {
        content: [
          {
            type: "text",
            text: res.stdout || `Proposed merge for hunk ${params.hunk_id} in ${params.file}.`,
          },
        ],
        details: {
          file: params.file,
          hunk_id: params.hunk_id,
          rationale: params.rationale ?? null,
        },
      };
    },
  });


  // ---------------------------------------------------------------------------
  // 1c. apply_recipe — run a NAMED, versioned COMPOSITE recipe (§G2): an ordered
  //     operator sequence from shuttle's built-in registry that lowers into a
  //     BatchSpec and runs through the EXACT SAME transactional gate as a batch
  //     (per-step confidence gate, composed full-file reparse, snapshot +
  //     rollback). A recipe adds NO new transformation power — prefer it over
  //     hand-chaining several apply_refactor calls when a named playbook fits.
  //     Discover with list:true; bind params with set:{k:v}; a required param
  //     with no default that you omit is a hard error (never guessed). Flow
  //     mirrors apply_refactor: a --json dry-run first (the gate runs here too,
  //     so a Low-confidence step is refused unless force:true), then --write
  //     unless preview_only:true.
  // ---------------------------------------------------------------------------
  pi.registerTool({
    name: "apply_recipe",
    label: "Apply Recipe",
    description:
      "Run a NAMED composite recipe (§G2) — a versioned, ordered operator " +
      "sequence from shuttle's built-in registry that lowers into a batch and runs " +
      "through the SAME transactional gate (per-step confidence gate, composed " +
      "reparse, atomic rollback). Prefer this over hand-chaining multiple " +
      "apply_refactor calls when a named playbook fits. Set list:true to " +
      "enumerate recipes (name/version/description/params incl. required). To " +
      "run one: pass name + file + set:{param:value}. A required param with no " +
      "default that you omit is a hard error (never guessed). We dry-run with " +
      "--json first (the gate runs here, so a Low-confidence step is refused " +
      "unless force:true), then write unless preview_only:true. expect_version " +
      "guards against a drifted playbook (refuses if the registry version differs). Pass from_file instead of name to load an out-of-tree *.recipe.json (mutually exclusive with name; same gate + caps).",
    parameters: Type.Object({
      name: Type.Optional(Type.String({ description: "Recipe name, e.g. 'perf.promote-sync-tunables'. Required unless list:true or from_file is given." })),
      from_file: Type.Optional(Type.String({ description: "Load an out-of-tree *.recipe.json from this path instead of a built-in registry recipe. Mutually exclusive with name. Same transactional gate + untrusted-input caps apply." })),
      file: Type.Optional(Type.String({ description: "The file the recipe's operators target. Required unless list:true." })),
      set: Type.Optional(
        Type.Record(Type.String(), Type.String(), {
          description: "Bind recipe params: {param: value}. Values are literals substituted into the recipe's templates.",
        }),
      ),
      expect_version: Type.Optional(Type.Number({ description: "Refuse to run unless the resolved recipe is exactly this version." })),
      list: Type.Optional(Type.Boolean({ description: "List the built-in recipe registry instead of running one. Default false." })),
      preview_only: Type.Optional(Type.Boolean({ description: "Only dry-run (no write). Default false (dry-run then write)." })),
      force: Type.Optional(Type.Boolean({ description: "Commit even if a step is Low-confidence (§9.5 gate override). Default false." })),
      shuttle_bin: Type.Optional(Type.String({ description: "Path to the `shuttle` binary." })),
    }),
    async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
      const bin = eaBin(params.shuttle_bin);
      const runEa = (argv: string[]) =>
        spawnSync(bin, argv, { encoding: "utf-8", maxBuffer: 32 * 1024 * 1024 });

      // ---- list mode: enumerate the registry, no file needed. ----
      if (params.list) {
        const ls = runEa(["refactor", "recipe", "--list", "--json"]);
        if (ls.error) return launchError(bin, ls.error.message);
        if (ls.status !== 0) return exitError(ls.status, ls.stderr);
        let recipes: unknown = [];
        try { recipes = JSON.parse(ls.stdout); } catch { recipes = []; }
        return {
          content: [{ type: "text", text: ls.stdout || "[]" }],
          details: { recipes },
        };
      }

      // ---- run mode: (name XOR from_file) + file required (validated here, not via a clap error). ----
      if (params.name && params.from_file) {
        return { content: [{ type: "text", text: "apply_recipe: `name` and `from_file` are mutually exclusive — pass exactly one." }], details: { conflict: "name+from_file" }, isError: true };
      }
      if (!params.name && !params.from_file) {
        return { content: [{ type: "text", text: "apply_recipe needs `name` (or `from_file`, or set list:true to enumerate)." }], details: { missing: "name" }, isError: true };
      }
      const label = params.name ?? params.from_file!;
      if (!params.file) {
        return { content: [{ type: "text", text: `apply_recipe '${label}' needs \`file\` (the file its operators target).` }], details: { missing: "file" }, isError: true };
      }

      const base = params.from_file
        ? ["refactor", "recipe", "--from-file", params.from_file, "--file", params.file]
        : ["refactor", "recipe", label, "--file", params.file];
      for (const [k, v] of Object.entries(params.set ?? {})) base.push("--set", `${k}=${v}`);
      if (params.expect_version !== undefined) base.push("--expect-version", String(params.expect_version));

      // ---- Phase 1: --json dry-run. The batch gate runs HERE too, so a
      //      Low-confidence step (or unresolved param / reparse failure) exits
      //      non-zero. `force` is threaded so a forced caller can still preview. ----
      const pv = runEa(params.force ? [...base, "--force", "--json"] : [...base, "--json"]);
      if (pv.error) return launchError(bin, pv.error.message);
      if (pv.status !== 0) {
        const err = (pv.stderr || "").trim();
        const lowBlocked = /Low-confidence/i.test(err) && !params.force;
        recordTelemetry({ tool: "apply_recipe", op: label, opCount: 0, outcome: lowBlocked ? "low_blocked" : "rejected", wrote: false, error: !lowBlocked });
        const hint = lowBlocked ? " Resubmit with force:true to apply." : "";
        return {
          content: [{ type: "text", text: `Recipe '${label}' ${lowBlocked ? "blocked (Low-confidence)" : "rejected"} — nothing written:\n${err}${hint}` }],
          details: { recipe: label, wrote: false, blocked: lowBlocked, stderr: err },
          isError: true,
        };
      }

      let preview: Record<string, unknown> = {};
      try { preview = JSON.parse(pv.stdout); } catch { preview = {}; }
      const steps = Array.isArray(preview.steps) ? (preview.steps as string[]) : [];
      const changed = Array.isArray(preview.changed)
        ? (preview.changed as Array<{ path?: string }>).map((c) => c.path).filter((p): p is string => !!p)
        : [];

      // ---- Preview-only: stop after the gate-passing dry-run. ----
      if (params.preview_only) {
        recordTelemetry({ tool: "apply_recipe", op: label, opCount: steps.length, outcome: "preview_only", wrote: false });
        return {
          content: [
            { type: "text", text: `Recipe '${label}': ${steps.length} step(s), ${changed.length} file(s) would change. Preview only — nothing written.` },
            { type: "text", text: JSON.stringify({ steps, changed }, null, 2) },
          ],
          details: { recipe: label, wrote: false, steps, changed, preview },
        };
      }

      // ---- Phase 2: commit. --write (+ --force if the caller allowed it). ----
      const wr = runEa(params.force ? [...base, "--write", "--force", "--json"] : [...base, "--write", "--json"]);
      if (wr.error) return launchError(bin, wr.error.message);
      if (wr.status !== 0) {
        recordTelemetry({ tool: "apply_recipe", op: label, opCount: steps.length, outcome: "write_failed", wrote: false, error: true });
        return exitError(wr.status, wr.stderr);
      }
      let out: Record<string, unknown> = {};
      try { out = JSON.parse(wr.stdout); } catch { out = {}; }
      const wroteSteps = Array.isArray(out.steps) ? (out.steps as string[]) : steps;
      const wroteChanged = Array.isArray(out.changed)
        ? (out.changed as Array<{ path?: string }>).map((c) => c.path).filter((p): p is string => !!p)
        : changed;
      recordTelemetry({ tool: "apply_recipe", op: label, opCount: wroteSteps.length, outcome: "applied", wrote: true });
      return {
        content: [
          { type: "text", text: `Recipe '${label}' applied: ${wroteSteps.length} step(s), ${wroteChanged.length} file(s) written.` },
          { type: "text", text: JSON.stringify({ steps: wroteSteps, changed: wroteChanged }, null, 2) },
        ],
        details: { recipe: label, wrote: true, steps: wroteSteps, changed: wroteChanged, out },
      };
    },
  });

}

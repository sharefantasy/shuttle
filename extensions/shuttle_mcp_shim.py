#!/usr/bin/env python3
"""shuttle_mcp_shim.py — zero-dependency stdio MCP server exposing shuttle fast-path tools.

Mirrors extensions/shuttle-tools.ts: each MCP tool shells out to the `shuttle` Rust binary
via subprocess and returns its structured stdout. Speaks JSON-RPC 2.0 over stdio.

Env:
  SHUTTLE_BIN             path to shuttle binary (else `shuttle` on PATH)
  SHUTTLE_ENFORCE_SCOPE   "1" to append --enforce-scope + --allowed-path flags
  SHUTTLE_ALLOWED_PATHS   comma-separated allowed paths (used when scope enforced)
"""
import json
import hashlib
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile

PROTOCOL_VERSION = "2024-11-05"
SERVER_NAME = "shuttle-fastpath"
SERVER_VERSION = "0.1.0"
MAXBUF = 32 * 1024 * 1024
SYMBOL_FOOTPRINT_SCHEMA = "ea_symbol_footprint/v1"
HYBRID_PARALLEL_ADVICE_SCHEMA = "ea_goal_parallel_advice/v2"
HYBRID_DAG_POLICY_VERSION = "ea_hybrid_dag_policy/v1"
PARALLEL_ADVICE_V1_SCHEMA = "ea_goal_parallel_advice/v1"
_DIGEST_RE = re.compile(r"sha256:[0-9a-f]{64}\Z")
_OID_RE = re.compile(r"(?:[0-9a-f]{40}|[0-9a-f]{64})\Z")
_AUTHORITY_KEYS = frozenset(
    {
        "approval",
        "claim",
        "credential",
        "dispatch",
        "fencing_token",
        "land",
        "push",
        "workspace",
        "write_scope",
    }
)


def shuttle_bin(explicit=None):
    env = os.environ.get("SHUTTLE_BIN")
    if explicit:
        # Some CLI clients may populate tool arguments with the bare default `"shuttle"`. Treat
        # that as a placeholder so a repo-local SHUTTLE_BIN env can still take
        # precedence; preserve genuinely explicit alternate paths/binaries.
        if explicit == "shuttle" and env:
            return env
        # Some prompts/tools hand back a guessed absolute path that does not
        # exist on this machine. If we already injected a known-good SHUTTLE_BIN via
        # the MCP server env, prefer that over a dead hint.
        if env and explicit != env and not os.path.exists(explicit):
            return env
        return explicit
    if env:
        return env
    found = shutil.which("shuttle")
    return found or "shuttle"


def _run(args, stdin_text=None, root=None):
    """Run shuttle with args; return (ok, stdout, stderr, code)."""
    try:
        proc = subprocess.run(
            args,
            input=stdin_text,
            capture_output=True,
            text=True,
            timeout=600,
        )
    except FileNotFoundError as e:
        return (False, "", "launch_error: %s" % e, -1)
    except subprocess.TimeoutExpired as e:
        return (False, "", "timeout: %s" % e, -1)
    out = proc.stdout or ""
    if len(out) > MAXBUF:
        out = out[:MAXBUF]
    return (proc.returncode == 0, out, proc.stderr or "", proc.returncode)


def _text_result(text):
    return {"content": [{"type": "text", "text": text}]}


def _launch_error(msg):
    return {"content": [{"type": "text", "text": "shuttle launch error: %s" % msg}], "isError": True}


def _exit_error(code, stderr, stdout):
    detail = stderr.strip() or stdout.strip() or "(no output)"
    return {
        "content": [{"type": "text", "text": "shuttle exited %s: %s" % (code, detail)}],
        "isError": True,
    }


def _scope_flags():
    flags = []
    if os.environ.get("SHUTTLE_ENFORCE_SCOPE") == "1":
        flags.append("--enforce-scope")
        allowed = os.environ.get("SHUTTLE_ALLOWED_PATHS", "")
        for p in [x.strip() for x in allowed.split(",") if x.strip()]:
            flags.extend(["--allowed-path", p])
    return flags


def _scope_root(requested):
    if os.environ.get("SHUTTLE_ENFORCE_SCOPE") == "1":
        return os.environ.get("SHUTTLE_WORKSPACE_ROOT", ".")
    return requested or "."


def _adapter_error(message):
    return {
        "content": [{"type": "text", "text": "SHUTTLE compatibility adapter rejected payload: %s" % message}],
        "isError": True,
    }


def _canonical_json_bytes(value):
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")


def _canonical_digest(value):
    return "sha256:" + hashlib.sha256(_canonical_json_bytes(value)).hexdigest()


def _closed_object(value, required, optional=(), label="object"):
    if not isinstance(value, dict):
        raise ValueError("%s must be an object" % label)
    actual = set(value)
    required = set(required)
    optional = set(optional)
    missing = sorted(required - actual)
    unknown = sorted(actual - required - optional)
    if missing:
        raise ValueError("%s is missing fields: %s" % (label, ", ".join(missing)))
    if unknown:
        raise ValueError("%s has unknown fields: %s" % (label, ", ".join(unknown)))
    return value


def _array(value, label):
    if not isinstance(value, list):
        raise ValueError("%s must be an array" % label)
    return value


def _text(value, label):
    if (
        not isinstance(value, str)
        or not value
        or value.strip() != value
        or len(value) > 4096
        or any(ord(char) < 32 or ord(char) == 127 for char in value)
    ):
        raise ValueError("%s must be non-empty, trimmed and control-free" % label)
    return value


def _digest(value, label):
    if not isinstance(value, str) or _DIGEST_RE.fullmatch(value) is None:
        raise ValueError("%s must be sha256:<64 lowercase hex>" % label)
    return value


def _oid(value, label):
    if not isinstance(value, str) or _OID_RE.fullmatch(value) is None:
        raise ValueError("%s must be a full lowercase hex OID" % label)
    return value


def _enum(value, allowed, label):
    if value not in allowed:
        raise ValueError("%s has unsupported value: %r" % (label, value))
    return value


def _sorted_unique(values, label, key=None):
    keys = [key(value) if key else value for value in values]
    if any(left >= right for left, right in zip(keys, keys[1:])):
        raise ValueError("%s must be strictly sorted and unique" % label)


_EVIDENCE_KIND_ORDER = {"import_directed": 0, "contract_directed": 1, "name_match": 2, "co_change": 3}
_CONFLICT_KIND_ORDER = {
    "reverse_model_edge": 0, "cycle_producing_addition": 1, "model_cycle": 2,
    "write_write_collision": 3, "ambiguous_evidence": 4, "invalid_task_reference": 5,
}
_PROVENANCE_ORDER = {"model": 0, "trusted_mechanical": 1, "both": 2}
_UNRESOLVED_REASON_ORDER = {
    "ambiguous": 0, "unsupported": 1, "stale": 2, "absent": 3,
    "timeout": 4, "overflow": 5, "invalid_declaration": 6, "not_found": 7,
}


def _option_key(value, key=lambda item: item):
    return (0,) if value is None else (1, key(value))


def _symbol_key(item):
    return (
        item["symbol_id"], item["repo_id"], item["base_oid"], item["language"],
        item["normalized_path"], item["name"], item["kind"],
        _option_key(item.get("scope")), item["line"], item["signature_digest"],
    )


def _candidate_key(item):
    return (_edge_key(item["edge"]), tuple(_EVIDENCE_KIND_ORDER[kind] for kind in item["kinds"]), tuple(item["evidence_refs"]))


def _weak_hint_key(item):
    return (item["task_a"], item["task_b"], _EVIDENCE_KIND_ORDER[item["kind"]], item["evidence_ref"])


def _conflict_key(item):
    return (
        _CONFLICT_KIND_ORDER[item["kind"]], tuple(item["task_ids"]),
        _option_key(item.get("edge"), _edge_key), _option_key(item.get("symbol_id")),
        tuple(item.get("evidence_refs", [])),
    )


def _hard_edge_key(item):
    return (
        _edge_key(item["edge"]), _PROVENANCE_ORDER[item["provenance"]],
        tuple(_EVIDENCE_KIND_ORDER[kind] for kind in item.get("kinds", [])),
        tuple(item.get("evidence_refs", [])),
    )


def _validate_no_authority(value, label="payload"):
    if isinstance(value, dict):
        forbidden = sorted(set(value) & _AUTHORITY_KEYS)
        if forbidden:
            raise ValueError("%s contains forbidden authority fields: %s" % (label, ", ".join(forbidden)))
        for key, child in value.items():
            _validate_no_authority(child, "%s.%s" % (label, key))
    elif isinstance(value, list):
        for index, child in enumerate(value):
            _validate_no_authority(child, "%s[%s]" % (label, index))


def _validate_symbol_identity(value, label):
    obj = _closed_object(
        value,
        (
            "symbol_id", "repo_id", "base_oid", "language",
            "normalized_path", "name", "kind", "line",
            "signature_digest",
        ),
        ("scope",),
        label,
    )
    _digest(obj["symbol_id"], label + ".symbol_id")
    _text(obj["repo_id"], label + ".repo_id")
    _oid(obj["base_oid"], label + ".base_oid")
    _text(obj["language"], label + ".language")
    path = _text(obj["normalized_path"], label + ".normalized_path")
    if (
        path.startswith("/")
        or "\\" in path
        or any(part in ("", ".", "..") for part in path.split("/"))
        or (len(path) > 1 and path[1] == ":")
    ):
        raise ValueError(label + ".normalized_path must be normalized and repository-relative")
    _text(obj["name"], label + ".name")
    _text(obj["kind"], label + ".kind")
    if "scope" in obj:
        _text(obj["scope"], label + ".scope")
    if isinstance(obj["line"], bool) or not isinstance(obj["line"], int) or obj["line"] < 1:
        raise ValueError(label + ".line must be a one-based integer")
    _digest(obj["signature_digest"], label + ".signature_digest")
    payload = {key: item for key, item in obj.items() if key != "symbol_id"}
    if obj["symbol_id"] != _canonical_digest(payload):
        raise ValueError(label + ".symbol_id does not match its canonical typed identity")
    return obj


def _validate_unresolved(value, label):
    obj = _closed_object(value, ("declaration", "reason"), label=label)
    _text(obj["declaration"], label + ".declaration")
    _enum(
        obj["reason"],
        {"ambiguous", "unsupported", "stale", "absent", "timeout", "overflow", "invalid_declaration", "not_found"},
        label + ".reason",
    )
    return obj


def _validate_symbol_list(values, label, repositories):
    result = _array(values, label)
    for index, symbol in enumerate(result):
        symbol = _validate_symbol_identity(symbol, "%s[%s]" % (label, index))
        if (symbol["repo_id"], symbol["base_oid"]) not in repositories:
            raise ValueError("%s[%s] has no matching repository binding" % (label, index))
    _sorted_unique(result, label, key=_symbol_key)
    return result


def _validate_symbol_footprint(value):
    obj = _closed_object(value, ("schema_version", "basis", "tasks"), label="symbol_footprint")
    if obj["schema_version"] != SYMBOL_FOOTPRINT_SCHEMA:
        raise ValueError("unsupported symbol footprint schema_version: %r" % obj["schema_version"])
    _validate_no_authority(obj, "symbol_footprint")
    basis = _closed_object(
        obj["basis"],
        ("repositories", "symbol_index_ref", "symbol_index_digest", "analyzer_version", "coverage", "freshness", "truncated"),
        ("degraded_reasons",),
        "symbol_footprint.basis",
    )
    repositories = set()
    repository_list = _array(basis["repositories"], "symbol_footprint.basis.repositories")
    if not repository_list:
        raise ValueError("symbol_footprint.basis.repositories must not be empty")
    for repository in repository_list:
        repository = _closed_object(repository, ("repo_id", "base_oid"), label="repository")
        binding = (_text(repository["repo_id"], "repository.repo_id"), _oid(repository["base_oid"], "repository.base_oid"))
        if binding in repositories:
            raise ValueError("repository bindings must be unique")
        repositories.add(binding)
    _sorted_unique(repository_list, "repositories", key=lambda item: (item["repo_id"], item["base_oid"]))
    _text(basis["symbol_index_ref"], "symbol_index_ref")
    _digest(basis["symbol_index_digest"], "symbol_index_digest")
    _text(basis["analyzer_version"], "analyzer_version")
    coverage = _enum(basis["coverage"], {"none", "partial", "exact"}, "coverage")
    freshness = _enum(basis["freshness"], {"fresh", "stale", "absent", "unsupported", "timeout"}, "freshness")
    if not isinstance(basis["truncated"], bool):
        raise ValueError("truncated must be boolean")
    reasons = _array(basis.get("degraded_reasons", []), "degraded_reasons")
    for reason in reasons:
        _text(reason, "degraded_reason")
    _sorted_unique(reasons, "degraded_reasons")
    if coverage == "exact" and (freshness != "fresh" or basis["truncated"]):
        raise ValueError("exact coverage requires a fresh, non-truncated symbol index")
    if (freshness != "fresh" or basis["truncated"]) and not reasons:
        raise ValueError("non-fresh or truncated footprint requires degraded_reasons")

    tasks = _array(obj["tasks"], "symbol_footprint.tasks")
    for index, task in enumerate(tasks):
        task_label = "symbol_footprint.tasks[%s]" % index
        task = _closed_object(task, ("task_id", "planned", "observed", "grants_path_authority"), label=task_label)
        _text(task["task_id"], task_label + ".task_id")
        if task["grants_path_authority"] is not False:
            raise ValueError(task_label + ".grants_path_authority must be false")
        planned = _closed_object(task["planned"], ("evidence_source", "declared_read_symbols", "declared_write_symbols", "unresolved"), label=task_label + ".planned")
        source = _enum(planned["evidence_source"], {"planner_declared_validated", "planner_declared_unresolved", "planner_guess"}, task_label + ".planned.evidence_source")
        reads = _validate_symbol_list(planned["declared_read_symbols"], task_label + ".planned.declared_read_symbols", repositories)
        writes = _validate_symbol_list(planned["declared_write_symbols"], task_label + ".planned.declared_write_symbols", repositories)
        unresolved = _array(planned["unresolved"], task_label + ".planned.unresolved")
        for offset, item in enumerate(unresolved):
            _validate_unresolved(item, "%s.planned.unresolved[%s]" % (task_label, offset))
        _sorted_unique(unresolved, task_label + ".planned.unresolved", key=lambda item: (item["declaration"], _UNRESOLVED_REASON_ORDER[item["reason"]]))
        if source == "planner_declared_validated" and unresolved:
            raise ValueError("planner_declared_validated must not contain unresolved declarations")
        if source == "planner_declared_unresolved" and (not unresolved or reads or writes):
            raise ValueError("planner_declared_unresolved must contain only unresolved declarations")
        if source == "planner_guess" and (reads or writes):
            raise ValueError("planner_guess must not contain exact symbols")
        observed = _closed_object(task["observed"], ("evidence_source", "observed_write_symbols", "unresolved"), label=task_label + ".observed")
        observed_source = _enum(observed["evidence_source"], {"not_observed", "observed_diff"}, task_label + ".observed.evidence_source")
        observed_writes = _validate_symbol_list(observed["observed_write_symbols"], task_label + ".observed.observed_write_symbols", repositories)
        observed_unresolved = _array(observed["unresolved"], task_label + ".observed.unresolved")
        for offset, item in enumerate(observed_unresolved):
            _validate_unresolved(item, "%s.observed.unresolved[%s]" % (task_label, offset))
        _sorted_unique(observed_unresolved, task_label + ".observed.unresolved", key=lambda item: (item["declaration"], _UNRESOLVED_REASON_ORDER[item["reason"]]))
        if observed_source == "not_observed" and (observed_writes or observed_unresolved):
            raise ValueError("not_observed must not contain observed evidence")
    _sorted_unique(tasks, "symbol_footprint.tasks", key=lambda item: item["task_id"])
    return obj


def _validate_artifact_binding(value, label):
    obj = _closed_object(value, ("ref", "digest"), label=label)
    digest = _digest(obj["digest"], label + ".digest")
    expected_ref = "artifact://sha256/" + digest[len("sha256:") :]
    if obj["ref"] != expected_ref:
        raise ValueError(label + ".ref must be the content address of its digest")
    return obj


def _validate_edge(value, label):
    obj = _closed_object(value, ("from", "to"), label=label)
    _text(obj["from"], label + ".from")
    _text(obj["to"], label + ".to")
    if obj["from"] == obj["to"]:
        raise ValueError(label + " must not be a self edge")
    return obj


def _edge_key(value):
    return (value["from"], value["to"])


def _validate_hybrid_advice(value, expected_footprint_digest=None):
    fields = (
        "schema_version", "policy_version", "planning_basis", "source_intent",
        "repository_bases", "symbol_footprint", "symbol_index_bindings",
        "model_edges", "trusted_addition_candidates", "weak_hints", "conflicts",
        "proposed_hard_edges", "schedule", "degraded",
    )
    obj = _closed_object(value, fields, label="parallel_advice")
    if obj["schema_version"] != HYBRID_PARALLEL_ADVICE_SCHEMA:
        raise ValueError("unsupported advice schema_version: %r" % obj["schema_version"])
    if obj["policy_version"] != HYBRID_DAG_POLICY_VERSION:
        raise ValueError("unsupported policy_version: %r" % obj["policy_version"])
    _validate_no_authority(obj, "parallel_advice")
    planning = _closed_object(obj["planning_basis"], ("planning_ref", "planning_revision", "planning_structure_digest"), label="planning_basis")
    _text(planning["planning_ref"], "planning_ref")
    _text(planning["planning_revision"], "planning_revision")
    _digest(planning["planning_structure_digest"], "planning_structure_digest")
    _validate_artifact_binding(obj["source_intent"], "source_intent")
    footprint_binding = _validate_artifact_binding(obj["symbol_footprint"], "symbol_footprint")
    if expected_footprint_digest is not None and footprint_binding["digest"] != expected_footprint_digest:
        raise ValueError("symbol_footprint.digest does not match the canonical input footprint")

    repositories = _array(obj["repository_bases"], "repository_bases")
    if not repositories:
        raise ValueError("repository_bases must not be empty")
    repo_ids = set()
    for repository in repositories:
        repository = _closed_object(repository, ("repo_id", "base_oid"), label="repository_base")
        repo_id = _text(repository["repo_id"], "repository_base.repo_id")
        _oid(repository["base_oid"], "repository_base.base_oid")
        if repo_id in repo_ids:
            raise ValueError("repository_bases repo_id must be unique")
        repo_ids.add(repo_id)
    _sorted_unique(repositories, "repository_bases", key=lambda item: (item["repo_id"], item["base_oid"]))
    indexes = _array(obj["symbol_index_bindings"], "symbol_index_bindings")
    for index in indexes:
        index = _closed_object(index, ("repo_id", "index_digest"), label="symbol_index_binding")
        if _text(index["repo_id"], "symbol_index_binding.repo_id") not in repo_ids:
            raise ValueError("symbol index binding has no repository base")
        _digest(index["index_digest"], "symbol_index_binding.index_digest")
    _sorted_unique(indexes, "symbol_index_bindings", key=lambda item: (item["repo_id"], item["index_digest"]))

    model_edges = _array(obj["model_edges"], "model_edges")
    for index, edge in enumerate(model_edges):
        _validate_edge(edge, "model_edges[%s]" % index)
    _sorted_unique(model_edges, "model_edges", key=_edge_key)
    model = {_edge_key(edge) for edge in model_edges}

    candidates = _array(obj["trusted_addition_candidates"], "trusted_addition_candidates")
    candidate_edges = set()
    for index, candidate in enumerate(candidates):
        candidate = _closed_object(candidate, ("edge", "kinds", "evidence_refs"), label="trusted_candidate")
        edge = _validate_edge(candidate["edge"], "trusted_candidate.edge")
        kinds = _array(candidate["kinds"], "trusted_candidate.kinds")
        if not kinds or any(kind not in ("import_directed", "contract_directed") for kind in kinds):
            raise ValueError("trusted candidates require trusted directed kinds")
        _sorted_unique(kinds, "trusted_candidate.kinds")
        refs = _array(candidate["evidence_refs"], "trusted_candidate.evidence_refs")
        if not refs:
            raise ValueError("trusted candidates require evidence_refs")
        for ref in refs:
            _text(ref, "trusted_candidate.evidence_ref")
        _sorted_unique(refs, "trusted_candidate.evidence_refs")
        candidate_edges.add(_edge_key(edge))
    _sorted_unique(candidates, "trusted_addition_candidates", key=_candidate_key)

    weak_hints = _array(obj["weak_hints"], "weak_hints")
    for hint in weak_hints:
        hint = _closed_object(hint, ("task_a", "task_b", "kind", "evidence_ref"), label="weak_hint")
        task_a = _text(hint["task_a"], "weak_hint.task_a")
        task_b = _text(hint["task_b"], "weak_hint.task_b")
        if task_a >= task_b or hint["kind"] not in ("name_match", "co_change"):
            raise ValueError("weak hints require a sorted task pair and weak kind")
        _text(hint["evidence_ref"], "weak_hint.evidence_ref")
    _sorted_unique(weak_hints, "weak_hints", key=_weak_hint_key)

    conflicts = _array(obj["conflicts"], "conflicts")
    conflict_kinds = {"reverse_model_edge", "cycle_producing_addition", "model_cycle", "write_write_collision", "ambiguous_evidence", "invalid_task_reference"}
    for conflict in conflicts:
        conflict = _closed_object(conflict, ("kind", "task_ids"), ("edge", "symbol_id", "evidence_refs"), "conflict")
        _enum(conflict["kind"], conflict_kinds, "conflict.kind")
        task_ids = _array(conflict["task_ids"], "conflict.task_ids")
        for task_id in task_ids:
            _text(task_id, "conflict.task_id")
        _sorted_unique(task_ids, "conflict.task_ids")
        if "edge" in conflict:
            _validate_edge(conflict["edge"], "conflict.edge")
        if "symbol_id" in conflict:
            _text(conflict["symbol_id"], "conflict.symbol_id")
        refs = _array(conflict.get("evidence_refs", []), "conflict.evidence_refs")
        for ref in refs:
            _text(ref, "conflict.evidence_ref")
        _sorted_unique(refs, "conflict.evidence_refs")
    _sorted_unique(conflicts, "conflicts", key=_conflict_key)

    hard_edges = _array(obj["proposed_hard_edges"], "proposed_hard_edges")
    hard = set()
    for item in hard_edges:
        item = _closed_object(item, ("edge", "provenance"), ("kinds", "evidence_refs"), "hard_edge")
        edge = _validate_edge(item["edge"], "hard_edge.edge")
        edge_key = _edge_key(edge)
        kinds = _array(item.get("kinds", []), "hard_edge.kinds")
        refs = _array(item.get("evidence_refs", []), "hard_edge.evidence_refs")
        if any(kind not in ("import_directed", "contract_directed") for kind in kinds):
            raise ValueError("weak evidence must never enter proposed_hard_edges")
        _sorted_unique(kinds, "hard_edge.kinds")
        for ref in refs:
            _text(ref, "hard_edge.evidence_ref")
        _sorted_unique(refs, "hard_edge.evidence_refs")
        provenance = _enum(item["provenance"], {"model", "trusted_mechanical", "both"}, "hard_edge.provenance")
        in_model = edge_key in model
        in_candidate = edge_key in candidate_edges
        valid = (
            (provenance == "model" and in_model and not in_candidate and not kinds and not refs)
            or (provenance == "trusted_mechanical" and not in_model and in_candidate and bool(kinds) and bool(refs))
            or (provenance == "both" and in_model and in_candidate and bool(kinds) and bool(refs))
        )
        if not valid:
            raise ValueError("hard edge provenance is inconsistent")
        hard.add(edge_key)
    _sorted_unique(hard_edges, "proposed_hard_edges", key=_hard_edge_key)
    if not model.issubset(hard):
        raise ValueError("every model edge must remain in proposed_hard_edges")

    schedule = _closed_object(obj["schedule"], ("batches", "max_width", "degraded"), ("serialized",), "schedule")
    batches = _array(schedule["batches"], "schedule.batches")
    batch_of = {}
    for position, batch in enumerate(batches):
        batch = _closed_object(batch, ("index", "tasks"), label="schedule.batch")
        tasks = _array(batch["tasks"], "schedule.batch.tasks")
        if batch["index"] != position or not tasks:
            raise ValueError("schedule batches must be non-empty and contiguously indexed")
        for task in tasks:
            _text(task, "schedule.task")
            if task in batch_of:
                raise ValueError("schedule task must occur exactly once")
            batch_of[task] = position
        _sorted_unique(tasks, "schedule.batch.tasks")
    if isinstance(schedule["max_width"], bool) or not isinstance(schedule["max_width"], int) or schedule["max_width"] != max((len(batch["tasks"]) for batch in batches), default=0):
        raise ValueError("schedule.max_width does not match batch width")
    if not isinstance(schedule["degraded"], bool):
        raise ValueError("schedule.degraded must be boolean")
    for edge_from, edge_to in hard:
        if edge_from not in batch_of or edge_to not in batch_of or batch_of[edge_from] >= batch_of[edge_to]:
            raise ValueError("schedule violates a hard edge")
    serialized = _array(schedule.get("serialized", []), "schedule.serialized")
    for reason in serialized:
        reason = _closed_object(reason, ("earlier", "later", "reason"), label="serialized_reason")
        _text(reason["reason"], "serialized_reason.reason")
        if reason["earlier"] not in batch_of or reason["later"] not in batch_of or batch_of[reason["earlier"]] >= batch_of[reason["later"]]:
            raise ValueError("serialized reason must point to a later batch")
    _sorted_unique(serialized, "schedule.serialized", key=lambda item: _canonical_json_bytes(item))

    degraded = _closed_object(obj["degraded"], ("value", "reasons"), label="degraded")
    reasons = _array(degraded["reasons"], "degraded.reasons")
    if not isinstance(degraded["value"], bool) or degraded["value"] != bool(reasons):
        raise ValueError("degraded.value must exactly reflect degraded.reasons")
    for reason in reasons:
        _text(reason, "degraded.reason")
    _sorted_unique(reasons, "degraded.reasons")
    if (conflicts or schedule["degraded"]) and not degraded["value"]:
        raise ValueError("conflicts or conservative scheduling require degraded=true")
    return obj


def _validate_v1_advice(value):
    obj = _closed_object(value, ("schema_version", "source_intent_ref", "source_intent_digest", "graph_basis", "batches", "serialized", "degraded", "max_width"), label="parallel_advice_v1")
    if obj["schema_version"] != PARALLEL_ADVICE_V1_SCHEMA:
        raise ValueError("unsupported advice schema_version: %r" % obj["schema_version"])
    _validate_no_authority(obj, "parallel_advice_v1")
    _text(obj["source_intent_ref"], "source_intent_ref")
    _digest(obj["source_intent_digest"], "source_intent_digest")
    basis = _closed_object(obj["graph_basis"], ("base_oid", "symbol_index_digest", "correction_digest", "footprint_source"), label="graph_basis")
    _oid(basis["base_oid"], "graph_basis.base_oid")
    _digest(basis["symbol_index_digest"], "graph_basis.symbol_index_digest")
    _digest(basis["correction_digest"], "graph_basis.correction_digest")
    if basis["footprint_source"] != "planner_guess":
        raise ValueError("v1 footprint_source must remain planner_guess")
    batches = _array(obj["batches"], "batches")
    batch_of = {}
    for position, batch in enumerate(batches):
        batch = _closed_object(batch, ("index", "task_ids"), label="batch")
        task_ids = _array(batch["task_ids"], "batch.task_ids")
        if batch["index"] != position or not task_ids:
            raise ValueError("v1 batches must be non-empty and contiguously indexed")
        for task_id in task_ids:
            _text(task_id, "batch.task_id")
            if task_id in batch_of:
                raise ValueError("v1 task_id must occur exactly once")
            batch_of[task_id] = position
    max_width = max((len(batch["task_ids"]) for batch in batches), default=0)
    if obj["max_width"] != max_width or not isinstance(obj["degraded"], bool):
        raise ValueError("v1 max_width/degraded fields are invalid")
    serialized = _array(obj["serialized"], "serialized")
    expected_degraded = False
    for reason in serialized:
        reason = _closed_object(reason, ("earlier", "later", "reason"), label="serialized_reason")
        _text(reason["reason"], "serialized_reason.reason")
        if reason["earlier"] not in batch_of or reason["later"] not in batch_of or batch_of[reason["earlier"]] >= batch_of[reason["later"]]:
            raise ValueError("v1 serialized reason must point to a later batch")
        expected_degraded = expected_degraded or reason["reason"].startswith("unprovable-independence")
    if obj["degraded"] != expected_degraded:
        raise ValueError("v1 degraded must exactly reflect unprovable serialization")
    return obj


def _pick(d, *keys):
    for key in keys:
        if key in d:
            return d[key]
    return None


def _normalize_plan_path(path, root):
    if not isinstance(path, str) or not path:
        return path
    try:
        abs_root = os.path.abspath(root)
        abs_path = os.path.abspath(path)
        if abs_path == abs_root:
            return "."
        prefix = abs_root.rstrip(os.sep) + os.sep
        if abs_path.startswith(prefix):
            return os.path.relpath(abs_path, abs_root)
    except Exception:
        return path
    return path


def _normalize_edit_entries(entries, fallback_path, root):
    ops = []
    for entry in entries or []:
        if not isinstance(entry, dict):
            return None
        path = _normalize_plan_path(_pick(entry, "path", "file") or fallback_path, root)
        line_hint = _pick(entry, "line_hint", "line")
        operations = entry.get("operations")
        replacements = entry.get("replacements")
        if path and isinstance(operations, list):
            for op_entry in operations:
                if not isinstance(op_entry, dict):
                    return None
                op_type = str(_pick(op_entry, "kind", "type", "op") or "").lower()
                if op_type == "replace":
                    old = _pick(op_entry, "old", "old_text", "search")
                    new = _pick(op_entry, "new", "new_text", "replace")
                    if old is None or new is None:
                        return None
                    op = {"kind": "search_replace", "path": path, "search": old, "replace": new}
                    if line_hint is not None:
                        op["line_hint"] = line_hint
                    ops.append(op)
                    continue
                if op_type == "insert":
                    anchor = _pick(op_entry, "anchor", "search")
                    position = _pick(op_entry, "position", "where")
                    content = op_entry.get("content")
                    if anchor is None or position is None or content is None:
                        return None
                    ops.append(
                        {
                            "kind": "insert",
                            "path": path,
                            "anchor": anchor,
                            "position": str(position).lower(),
                            "content": content,
                        }
                    )
                    continue
                return None
            continue
        if path and isinstance(replacements, list):
            for repl in replacements:
                if not isinstance(repl, dict):
                    return None
                old = _pick(repl, "old", "old_text", "search")
                new = _pick(repl, "new", "new_text", "replace")
                if old is None or new is None:
                    return None
                op = {"kind": "search_replace", "path": path, "search": old, "replace": new}
                if line_hint is not None:
                    op["line_hint"] = line_hint
                ops.append(op)
            continue
        old = _pick(entry, "old", "old_text", "search")
        new = _pick(entry, "new", "new_text", "replace")
        if path and old is not None and new is not None:
            op = {"kind": "search_replace", "path": path, "search": old, "replace": new}
            if line_hint is not None:
                op["line_hint"] = line_hint
            ops.append(op)
            continue
        if path and "anchor" in entry and "position" in entry and "content" in entry:
            ops.append(
                {
                    "kind": "insert",
                    "path": path,
                    "anchor": entry["anchor"],
                    "position": str(entry["position"]).lower(),
                    "content": entry["content"],
                }
            )
            continue
        if path and old is None and new is None and "content" in entry:
            ops.append({"kind": "create_file", "path": path, "content": entry["content"]})
            continue
        return None
    return ops


def _normalize_edit_plan(plan, root):
    if not isinstance(plan, dict):
        return plan

    if isinstance(plan.get("ops"), list):
        ops = _normalize_edit_entries(plan["ops"], _pick(plan, "path", "file"), root)
        if ops is None:
            normalized = dict(plan)
            normalized.pop("kind", None)
            normalized.setdefault("version", 1)
            return normalized
        normalized = {"version": plan.get("version", 1), "ops": ops}
        if "intent" in plan:
            normalized["intent"] = plan["intent"]
        return normalized

    ops = []
    if isinstance(plan.get("files"), list):
        for file_entry in plan["files"]:
            if not isinstance(file_entry, dict):
                return plan
            fallback_path = _pick(file_entry, "path", "file")
            if isinstance(file_entry.get("edits"), list):
                file_ops = _normalize_edit_entries(file_entry["edits"], fallback_path, root)
                if file_ops is None:
                    return plan
                ops.extend(file_ops)
                continue
            if fallback_path and "content" in file_entry:
                ops.append(
                    {
                        "kind": "create_file",
                        "path": _normalize_plan_path(fallback_path, root),
                        "content": file_entry["content"],
                    }
                )
                continue
            return plan
    elif isinstance(plan.get("edits"), list):
        ops = _normalize_edit_entries(plan["edits"], _pick(plan, "path", "file"), root)
        if ops is None:
            return plan

    if not ops:
        return plan

    normalized = {"version": plan.get("version", 1), "ops": ops}
    if "intent" in plan:
        normalized["intent"] = plan["intent"]
    return normalized


# ---- ToolOutcome envelope awareness (P1.3) -------------------------------
#
# Every shuttle tool lowers its native result into a universal `ToolOutcome`
# envelope (shuttle_protocol::tool_outcome). The envelope has three states:
#   green   -> done
#   blocked -> one-step-fixable (whats_missing + next_action); NOT a failure
#   error   -> genuine hard failure the tool cannot frame as a next action
#
# The MCP contract (§ tool_outcome.rs): map `isError: true` ONLY when the
# envelope's state == "error". A `blocked` result is actionable, so it comes
# back as a normal (non-error) result carrying the envelope so a router/model
# can read whats_missing/next_action and fix it in one step.


def _parse_outcome(out):
    """If stdout carries a ToolOutcome envelope, return it (dict); else None.

    Accepts either whole-stdout JSON or the last non-empty line (a tool may
    print human text then a JSON envelope tail). Identifies the envelope by its
    shape: an object with `state` + `reason_code` + `tool`.
    """
    if not out:
        return None
    s = out.strip()
    candidates = [s]
    lines = [ln.strip() for ln in s.splitlines() if ln.strip()]
    if lines:
        candidates.append(lines[-1])
    for cand in candidates:
        if not (cand.startswith("{") and cand.endswith("}")):
            continue
        try:
            obj = json.loads(cand)
        except (ValueError, TypeError):
            continue
        if isinstance(obj, dict) and "state" in obj and "reason_code" in obj and "tool" in obj:
            return obj
    return None


def _outcome_to_mcp(obj, raw):
    """Map a ToolOutcome envelope to an MCP tools/call result.

    isError is set ONLY when state == "error" (P1.3). green/blocked return as
    normal results; the full envelope is carried in the text so the caller can
    act on whats_missing/next_action/force_hint without parsing prose.
    """
    result = {"content": [{"type": "text", "text": raw}]}
    if obj.get("state") == "error":
        result["isError"] = True
    return result


def _finalize(ok, out, err, code):
    """Shared handler tail.

    Prefer a ToolOutcome envelope's `state` to decide isError; fall back to
    exit-code semantics for any tool not yet emitting an envelope. A launch
    failure (code == -1) is always an isError.
    """
    if code == -1:
        return _launch_error(err)
    outcome = _parse_outcome(out)
    if outcome is not None:
        return _outcome_to_mcp(outcome, out)
    if not ok:
        return _exit_error(code, err, out)
    return _text_result(out)


# ---- tool handlers -------------------------------------------------------

def tool_apply_edit_plan(params):
    bin_ = shuttle_bin(params.get("shuttle_bin"))
    root = _scope_root(params.get("root"))
    plan = params.get("plan")
    if plan is None:
        return _launch_error("missing 'plan'")
    plan = _normalize_edit_plan(plan, root)
    args = [bin_, "apply", "--root", root]
    if params.get("dry_run"):
        args.append("--dry-run")
    else:
        args.append("--gate")
    args.extend(_scope_flags())
    ok, out, err, code = _run(args, stdin_text=json.dumps(plan))
    return _finalize(ok, out, err, code)


def tool_shuttle_context_pack(params):
    bin_ = shuttle_bin(params.get("shuttle_bin"))
    root = params.get("root") or os.getcwd()
    args = [bin_, "context-pack", "--root", root]
    if params.get("budget") is not None:
        args.extend(["--budget", str(params["budget"])])
    if params.get("tasks_dir"):
        args.extend(["--tasks-dir", params["tasks_dir"]])
    if params.get("task_id"):
        args.extend(["--task-id", params["task_id"]])
    if params.get("depends_on"):
        args.extend(["--depends-on", ",".join(params["depends_on"])])
    if params.get("touch_hint"):
        args.extend(["--touch-hint", ",".join(params["touch_hint"])])
    if params.get("git_base"):
        args.extend(["--git-base", params["git_base"]])
    if params.get("symbol_index"):
        args.extend(["--symbol-index", params["symbol_index"]])
    if params.get("index_head"):
        args.extend(["--index-head", params["index_head"]])
    if params.get("write_symbols"):
        args.extend(["--write-symbol", ",".join(params["write_symbols"])])
    ok, out, err, code = _run(args)
    if code == -1 or not ok:
        return _finalize(ok, out, err, code)
    try:
        pack = json.loads(out)
        if not isinstance(pack, dict):
            raise ValueError("context-pack stdout must be a JSON object")
        footprint = pack.get("symbol_footprint")
        if footprint is not None:
            _validate_symbol_footprint(footprint)
    except (TypeError, ValueError) as exc:
        return _adapter_error("invalid context-pack typed output: %s" % exc)
    result = _text_result(json.dumps(pack, indent=2, sort_keys=True))
    result["details"] = {
        "symbol_footprint": footprint,
        "legacy_declaration_source": "planner_guess" if params.get("write_symbols") else None,
        "typed_symbol_evidence": footprint is not None,
        "grants_path_authority": False,
    }
    return result


def tool_shuttle_goal_parallel_advice(params):
    """Produce and validate read-only v1/v2 parallel advice.

    Default auto selects v2 when a canonical footprint is present and otherwise
    falls back to v1. The v2 footprint travels through a private canonical JSON
    file because the CLI contract is file-based. The directory is removed on
    every exit path.
    """
    bin_ = shuttle_bin(params.get("shuttle_bin"))
    root = params.get("root") or os.getcwd()
    plan = params.get("plan")
    requested_version = params.get("advice_version") or "auto"
    if not plan:
        return _adapter_error("missing 'plan'")
    if requested_version not in ("auto", "v1", "v2"):
        return _adapter_error("advice_version must be auto, v1 or v2")
    footprint = params.get("symbol_footprint")
    if requested_version == "v2" and footprint is None:
        return _adapter_error("v2 requires canonical symbol_footprint object")
    if requested_version == "v1" and footprint is not None:
        return _adapter_error("v1 does not accept symbol_footprint; it remains planner_guess")
    selected_version = (
        "v2" if footprint is not None else "v1"
    ) if requested_version == "auto" else requested_version
    try:
        footprint_digest = None
        if footprint is not None:
            _validate_symbol_footprint(footprint)
            footprint_digest = _canonical_digest(footprint)
    except (TypeError, ValueError) as exc:
        return _adapter_error("invalid symbol_footprint: %s" % exc)

    temp_dir = tempfile.mkdtemp(prefix="shuttle-advice-adapter-")
    try:
        advice_path = pathlib.Path(temp_dir) / "parallel-advice.json"
        args = [
            bin_, "goal", "--plan", plan, "--root", root, "--dry-run",
            "--parallel", "--advice-out", str(advice_path), "--json",
        ]
        if selected_version == "v2":
            footprint_path = pathlib.Path(temp_dir) / "symbol-footprint.json"
            footprint_path.write_bytes(_canonical_json_bytes(footprint) + b"\n")
            if requested_version == "v2":
                args.extend(["--advice-version", "v2"])
            args.extend(["--symbol-footprint", str(footprint_path)])
        elif requested_version == "v1":
            args.extend(["--advice-version", "v1"])
        ok, out, err, code = _run(args)
        if code == -1 or not ok:
            return _finalize(ok, out, err, code)
        try:
            advice = json.loads(advice_path.read_text(encoding="utf-8"))
            if selected_version == "v2":
                _validate_hybrid_advice(advice, footprint_digest)
            else:
                _validate_v1_advice(advice)
        except (OSError, TypeError, ValueError) as exc:
            return _adapter_error("invalid parallel advice output: %s" % exc)
        advice_digest = _canonical_digest(advice)
        return {
            "content": [{"type": "text", "text": json.dumps(advice, indent=2, sort_keys=True)}],
            "details": {
                "advice": advice,
                "advice_digest": advice_digest,
                "read_only": True,
                "authority": "none",
                "selected_advice_version": selected_version,
            },
        }
    finally:
        shutil.rmtree(temp_dir, ignore_errors=True)


def tool_shuttle_locate(params):
    bin_ = shuttle_bin(params.get("shuttle_bin"))
    root = params.get("root") or os.getcwd()
    pattern = params.get("pattern")
    if not pattern:
        return _launch_error("missing 'pattern'")
    args = [bin_, "locate", "--pattern", pattern, "--root", root]
    if params.get("lang"):
        args.extend(["--lang", params["lang"]])
    ok, out, err, code = _run(args)
    return _finalize(ok, out, err, code)


def tool_shuttle_repomap(params):
    bin_ = shuttle_bin(params.get("shuttle_bin"))
    root = params.get("root") or os.getcwd()
    args = [bin_, "repomap", "--root", root]
    if params.get("budget") is not None:
        args.extend(["--budget", str(params["budget"])])
    for f in params.get("focus", []) or []:
        args.extend(["--focus", f])
    ok, out, err, code = _run(args)
    return _finalize(ok, out, err, code)


def tool_shuttle_rename(params):
    bin_ = shuttle_bin(params.get("shuttle_bin"))
    root = params.get("root") or os.getcwd()
    frm = params.get("from")
    to = params.get("to")
    if not frm or not to:
        return _launch_error("missing 'from'/'to'")
    args = [bin_, "rename", "--from", frm, "--to", to, "--root", root]
    if params.get("dry_run"):
        args.append("--dry-run")
    ok, out, err, code = _run(args)
    return _finalize(ok, out, err, code)


def tool_conflict_propose(params):
    bin_ = shuttle_bin(params.get("shuttle_bin"))
    root = params.get("root") or os.getcwd()
    file = params.get("file")
    hunk_id = params.get("hunk_id")
    merged_text = params.get("merged_text")
    if not file or not hunk_id or merged_text is None:
        return _launch_error("missing 'file'/'hunk_id'/'merged_text'")
    for marker in ("<<<<<<<", "=======", ">>>>>>>"):
        for line in merged_text.splitlines():
            if line.startswith(marker):
                return _launch_error("merged_text still contains conflict marker: %s" % marker)
    args = [bin_, "conflict", "propose", file, hunk_id, "--text", merged_text, "--root", root]
    ok, out, err, code = _run(args)
    return _finalize(ok, out, err, code)


def tool_apply_recipe(params):
    """Run a NAMED composite recipe (G2) via `shuttle refactor recipe`.

    Mirrors extensions/shuttle-tools.ts apply_recipe: list mode enumerates the
    built-in registry; run mode dry-runs with --json (the batch gate runs here,
    so a Low-confidence step exits non-zero) then writes unless preview_only.
    """
    bin_ = shuttle_bin(params.get("shuttle_bin"))

    # ---- list mode: enumerate the registry, no file needed. ----
    if params.get("list"):
        ok, out, err, code = _run([bin_, "refactor", "recipe", "--list", "--json"])
        if code == -1:
            return _launch_error(err)
        if not ok:
            return _exit_error(code, err, out)
        return _text_result(out or "[]")

    # ---- run mode: (name OR from_file) + file required (validated here). ----
    name = params.get("name")
    from_file = params.get("from_file")
    file = params.get("file")
    if name and from_file:
        return _launch_error("apply_recipe: pass EITHER 'name' OR 'from_file', not both")
    if not name and not from_file:
        return _launch_error("apply_recipe needs 'name' or 'from_file' (or set list:true to enumerate)")
    if not file:
        label = name or from_file
        return _launch_error("apply_recipe '%s' needs 'file' (the file its operators target)" % label)

    if from_file:
        # External *.recipe.json: capped + linted through the same core as a
        # built-in recipe, then run the identical transactional gate.
        base = [bin_, "refactor", "recipe", "--from-file", from_file, "--file", file]
    else:
        base = [bin_, "refactor", "recipe", name, "--file", file]
    for k, v in (params.get("set") or {}).items():
        base.extend(["--set", "%s=%s" % (k, v)])
    if params.get("expect_version") is not None:
        base.extend(["--expect-version", str(params["expect_version"])])
    force = bool(params.get("force"))

    # ---- Phase 1: --json dry-run. The gate runs here too. ----
    pv_args = base + (["--force", "--json"] if force else ["--json"])
    ok, out, err, code = _run(pv_args)
    if code == -1:
        return _launch_error(err)
    if not ok:
        detail = (err or "").strip()
        low_blocked = ("low-confidence" in detail.lower()) and not force
        hint = " Resubmit with force:true to apply." if low_blocked else ""
        state = "blocked (Low-confidence)" if low_blocked else "rejected"
        label = name or from_file
        result = {
            "content": [{"type": "text", "text": "Recipe '%s' %s - nothing written:\n%s%s" % (label, state, detail, hint)}],
        }
        # P1.3 contract: a Low-confidence block is one-step-fixable (resubmit
        # with force:true), NOT a hard failure -> NOT isError. A genuine gate
        # rejection is unframeable -> isError.
        if not low_blocked:
            result["isError"] = True
        return result

    # ---- Preview-only: stop after the gate-passing dry-run. ----
    if params.get("preview_only"):
        return _text_result(out)

    # ---- Phase 2: commit. --write (+ --force if allowed). ----
    wr_args = base + (["--write", "--force", "--json"] if force else ["--write", "--json"])
    ok, out, err, code = _run(wr_args)
    return _finalize(ok, out, err, code)


def _ui_run(subcmd, params):
    """Shared tail for the seven `shuttle ui <verb>` evidence tools.

    Each verb reads a single closed-schema request document as JSON on stdin and
    prints the corresponding response document. The `request` param is the
    already-parsed JSON object; we re-serialise it and pipe it to `shuttle ui`.
    These verbs only reason and report — no workspace mutation, no scope flags.
    """
    bin_ = shuttle_bin(params.get("shuttle_bin"))
    request = params.get("request")
    if request is None:
        return _launch_error("missing 'request'")
    args = [bin_, "ui", subcmd]
    ok, out, err, code = _run(args, stdin_text=json.dumps(request))
    return _finalize(ok, out, err, code)


def tool_ui_verify(params):
    return _ui_run("verify", params)


def tool_ui_couple(params):
    return _ui_run("couple", params)


def tool_ui_render(params):
    return _ui_run("render", params)


def tool_ui_adopt(params):
    return _ui_run("adopt", params)


def tool_ui_assess(params):
    return _ui_run("assess", params)


def tool_ui_canvas(params):
    return _ui_run("canvas", params)


def tool_ui_receipt(params):
    return _ui_run("receipt", params)


# ---- tool registry (JSON Schemas mirror shuttle-tools.ts) ---------------------

TOOLS = [
    {
        "name": "apply_edit_plan",
        "description": "Apply a structured edit plan atomically through shuttle (gate/all-or-nothing). Prefer this over native edit for multi-file or scoped changes.",
        "handler": tool_apply_edit_plan,
        "inputSchema": {
            "type": "object",
            "properties": {
                "plan": {"type": "object", "description": "Edit plan JSON piped to `shuttle apply`."},
                "root": {"type": "string", "description": "Repo root (default cwd)."},
                "dry_run": {"type": "boolean", "description": "Preview only, no writes."},
                "shuttle_bin": {"type": "string"},
            },
            "required": ["plan"],
        },
    },
    {
        "name": "shuttle_context_pack",
        "description": "Build a token-budgeted context pack. Legacy NAME@PATH declarations stay planner_guess until Rust returns a canonical typed symbol_footprint.",
        "handler": tool_shuttle_context_pack,
        "inputSchema": {
            "type": "object",
            "properties": {
                "root": {"type": "string"},
                "budget": {"type": "integer"},
                "tasks_dir": {"type": "string"},
                "task_id": {"type": "string"},
                "depends_on": {"type": "array", "items": {"type": "string"}},
                "touch_hint": {"type": "array", "items": {"type": "string"}},
                "git_base": {"type": "string"},
                "symbol_index": {"type": "string"},
                "index_head": {"type": "string"},
                "write_symbols": {"type": "array", "items": {"type": "string"}},
                "shuttle_bin": {"type": "string"},
            },
        },
    },
    {
        "name": "shuttle_goal_parallel_advice",
        "description": "Produce read-only SHUTTLE parallel advice. v2 accepts and returns only closed canonical typed objects; v1 remains explicit planner_guess compatibility.",
        "handler": tool_shuttle_goal_parallel_advice,
        "inputSchema": {
            "type": "object",
            "additionalProperties": False,
            "properties": {
                "plan": {"type": "string", "description": "Path to an intent JSON plan."},
                "symbol_footprint": {
                    "type": "object",
                    "description": "Closed canonical ea_symbol_footprint/v1 object. Selects v2 under auto; required for explicit v2 and forbidden for explicit v1.",
                },
                "advice_version": {"type": "string", "enum": ["auto", "v1", "v2"], "description": "Default auto selects v2 when symbol_footprint is present, otherwise v1."},
                "root": {"type": "string"},
                "shuttle_bin": {"type": "string"},
            },
            "required": ["plan"],
        },
    },
    {
        "name": "shuttle_locate",
        "description": "Structurally locate symbols/definitions matching a pattern via `shuttle locate`.",
        "handler": tool_shuttle_locate,
        "inputSchema": {
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "root": {"type": "string"},
                "lang": {"type": "string"},
                "shuttle_bin": {"type": "string"},
            },
            "required": ["pattern"],
        },
    },
    {
        "name": "shuttle_repomap",
        "description": "Generate a token-budgeted repository map via `shuttle repomap`.",
        "handler": tool_shuttle_repomap,
        "inputSchema": {
            "type": "object",
            "properties": {
                "root": {"type": "string"},
                "budget": {"type": "integer"},
                "focus": {"type": "array", "items": {"type": "string"}},
                "shuttle_bin": {"type": "string"},
            },
        },
    },
    {
        "name": "shuttle_rename",
        "description": "Rename a symbol across the repo via `shuttle rename`.",
        "handler": tool_shuttle_rename,
        "inputSchema": {
            "type": "object",
            "properties": {
                "from": {"type": "string"},
                "to": {"type": "string"},
                "root": {"type": "string"},
                "dry_run": {"type": "boolean"},
                "shuttle_bin": {"type": "string"},
            },
            "required": ["from", "to"],
        },
    },
    {
        "name": "conflict_propose",
        "description": "Propose a merge resolution for a conflict hunk via `shuttle conflict propose` (rejects unresolved markers).",
        "handler": tool_conflict_propose,
        "inputSchema": {
            "type": "object",
            "properties": {
                "file": {"type": "string"},
                "hunk_id": {"type": "string"},
                "merged_text": {"type": "string"},
                "root": {"type": "string"},
                "shuttle_bin": {"type": "string"},
            },
            "required": ["file", "hunk_id", "merged_text"],
        },
    },
    {
        "name": "apply_recipe",
        "description": "Run a composite recipe (G2): a versioned, ordered operator sequence that lowers into a batch and runs through the SAME transactional gate. Use a built-in registry name, OR an out-of-tree *.recipe.json via from_file (capped + linted through the same core). Prefer over hand-chaining apply_refactor when a named playbook fits. Set list:true to enumerate the built-in registry; to run one pass name(or from_file)+file+set{param:value}.",
        "handler": tool_apply_recipe,
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Recipe name, e.g. 'perf.promote-sync-tunables'. Required unless list:true."},
                "file": {"type": "string", "description": "The file the recipe's operators target. Required unless list:true."},
                "from_file": {"type": "string", "description": "Path to an out-of-tree *.recipe.json to run instead of a built-in name (capped + linted through the same core, then the same gate). Mutually exclusive with name."},
                "set": {"type": "object", "description": "Bind recipe params: {param: value} literals.", "additionalProperties": {"type": "string"}},
                "expect_version": {"type": "integer", "description": "Refuse to run unless the resolved recipe is exactly this version."},
                "list": {"type": "boolean", "description": "List the built-in recipe registry instead of running one."},
                "preview_only": {"type": "boolean", "description": "Only dry-run (no write)."},
                "force": {"type": "boolean", "description": "Commit even if a step is Low-confidence."},
                "shuttle_bin": {"type": "string"},
            },
        },
    },
    {
        "name": "ui_verify",
        "description": "Evidence arm of the formal-UI-proposition surface: evaluate declared coord_atom/v1 propositions against a measured frame of bounding boxes, rolling up a three-valued (satisfied/violated/indeterminate) outcome. A geometric false on a proxy family (order/adjacency/alignment/overlap) degrades to null — a blind spot is NEVER laundered into a decided false. Read-only reasoning; no workspace mutation.",
        "handler": tool_ui_verify,
        "inputSchema": {
            "type": "object",
            "additionalProperties": False,
            "properties": {
                "request": {"type": "object", "description": "Closed VerifyRequest: {frame:{tolerance,boxes:[{element,axis,start,end}]}, atoms:[{family,relation,args,axes,allen?,threshold_ref?,frame,time_quantifier}], thresholds:{token://...:number}}."},
                "shuttle_bin": {"type": "string"},
            },
            "required": ["request"],
        },
    },
    {
        "name": "ui_couple",
        "description": "Constraint arm: L1 lexicon type-checks each dimension against its legal range; an L2 coupling graph classifies the arbitration complexity floor (lint vs Cassowary-class solver); a non-degradable a11y hard constraint fails closed (an unknown measurement BLOCKS, never passes). Returns per-dimension status, solver_class, per-a11y verdict, and overall admissibility. Read-only.",
        "handler": tool_ui_couple,
        "inputSchema": {
            "type": "object",
            "additionalProperties": False,
            "properties": {
                "request": {"type": "object", "description": "Closed CoupleRequest: {dimensions:[{name,value:{kind:metric|ordinal,...},range:{kind:interval|ordinal,...}}], couplings:[{from,to,kind:value|constraint|time}], a11y:[{name,measured?,required_min}]}."},
                "shuttle_bin": {"type": "string"},
            },
            "required": ["request"],
        },
    },
    {
        "name": "ui_render",
        "description": "Generation arm (Form A): emit deterministic HTML/CSS from a scene and RE-PROVE every declared atom against the rendered geometry — the render=verify loop. A decided false on a re-measured atom is a regression; a null is unverified (not a regression). Optionally binds the content-addressed artifact digest into a signed ui_projection/v1 when adopt_surface_id is set. Read-only reasoning + artifact synthesis; no workspace mutation.",
        "handler": tool_ui_render,
        "inputSchema": {
            "type": "object",
            "additionalProperties": False,
            "properties": {
                "request": {"type": "object", "description": "Closed RenderRequest: {scene:{width,height,tolerance,elements:[{id,x:[lo,hi],y:[lo,hi],text?,role?}]}, atoms:[...], thresholds:{...}, adopt_surface_id?:string, principles:[{name,formula,is_geometric_proxy}]}."},
                "shuttle_bin": {"type": "string"},
            },
            "required": ["request"],
        },
    },
    {
        "name": "ui_adopt",
        "description": "Bind an already-verified artifact digest, its atoms, and its design principles into a signed ui_projection/v1. A malformed digest, a dangling principle atom reference, or a degenerate frame fails closed. shuttle is only the evidence/judgment owner here — it never usurps campaign truth (red-line #11). Read-only.",
        "handler": tool_ui_adopt,
        "inputSchema": {
            "type": "object",
            "additionalProperties": False,
            "properties": {
                "request": {"type": "object", "description": "Closed AdoptRequest: {surface_id, artifact_digest:'sha256:<64hex>', frame:{...}, atoms:[...], principles:[{name,formula:{op:atom|all|any|not,...},is_geometric_proxy}], thresholds:{...}}."},
                "shuttle_bin": {"type": "string"},
            },
            "required": ["request"],
        },
    },
    {
        "name": "ui_assess",
        "description": "Form B arm (design \u00a74.2): reverse-measure an EXTERNAL pixel engine's produced artifact and gate it into an adapter_receipt/v1. A token \u786c\u95e8 met by the geometry adopts; unmet & non-droppable rejects; unmet & droppable degrades gracefully; a blind spot (no threshold) blocks (\u7edd\u4e0d\u778e\u731c). a11y / capability negotiation are hard (fail-closed on unknown/denied); the engine's aesthetic self-report is soft (below-floor only sidecars). shuttle NEVER generates the pixels itself (red-line #6); the disposition is computed by the receipt fold, never laundered into an adopt. Read-only.",
        "handler": tool_ui_assess,
        "inputSchema": {
            "type": "object",
            "additionalProperties": False,
            "properties": {
                "request": {"type": "object", "description": "Closed AssessRequest: {surface_id, engine_id, engine_version, artifact_digest:'sha256:<64hex>', frame:{tolerance,boxes:[{element,axis,start,end}]}, token_gates:[{name,atom,droppable}], a11y_gates?:[{name,measured?,required_min,droppable}], capability_gates?:[{name,advertised?,required,droppable}], mappings?:[{layer_id,token_ref,corroborating_atom?}], aesthetic?:{score,floor,rationale_ref}, extra_atoms?:[...], thresholds:{...}}."},
                "shuttle_bin": {"type": "string"},
            },
            "required": ["request"],
        },
    },
    {
        "name": "ui_canvas",
        "description": "Form C arm (design \u00a74.3): overlay a Form A SVG skeleton with an OPTIONAL already-adjudicated Form B adapter_receipt/v1 into ONE digest-bound canvas_underlay/v1 (\u8d4b\u503c + \u547d\u9898 + provenance) a host front-end renders into a Figma-class interactive canvas. The canvas is a RENDER PRODUCT, not a truth source (truth stays in the coord_atom propositions, red-line #7); shuttle builds neither the canvas UI nor the pixel engine (red-lines #6/#11) and never silently fails (\u00a74.2) \u2014 an inadmissible fidelity layer surfaces as blocked with NO laundered node bindings. Read-only reasoning; no workspace mutation.",
        "handler": tool_ui_canvas,
        "inputSchema": {
            "type": "object",
            "additionalProperties": False,
            "properties": {
                "request": {"type": "object", "description": "Closed ComposeRequest: {surface_id, scene:{width,height,tolerance,elements:[{id,x:[lo,hi],y:[lo,hi],text?,role?}]}, atoms:[...], thresholds:{...}, fidelity?:<an adapter_receipt/v1 object from ui_assess>}."},
                "shuttle_bin": {"type": "string"},
            },
            "required": ["request"],
        },
    },
    {
        "name": "ui_receipt",
        "description": "Evidence-owner emit path (design \u00a76.3): seal a verified ui_projection/v1 into a digest-bound pde_campaign_phase_receipt/v1. Runs the fail-closed seal gate \u2014 the projection must self-verify, and a decided effect on a non-satisfied (blind-spot) projection is refused (\u7edd\u4e0d\u778e\u731c) \u2014 and binds the receipt digest byte-compatibly with the pde kernel so pde can cross-verify without shuttle ever owning the campaign. shuttle mirrors the pde-campaign FROZEN schema read-only; it never writes the campaign store nor owns any land right (red-lines #7/#11). Read-only reasoning + atomic receipt emit; no workspace mutation.",
        "handler": tool_ui_receipt,
        "inputSchema": {
            "type": "object",
            "additionalProperties": False,
            "properties": {
                "request": {"type": "object", "description": "Closed UiPhaseReceiptRequest: {campaign_id, expected_campaign_revision, idempotency_key, action, from_phase, to_phase, owner, authority_artifact?, effect, decision?, observed_at, projection:<a verified ui_projection/v1 from ui_adopt/ui_render>}."},
                "shuttle_bin": {"type": "string"},
            },
            "required": ["request"],
        },
    },
]

TOOL_BY_NAME = {t["name"]: t for t in TOOLS}


def _tools_list_payload():
    return [
        {"name": t["name"], "description": t["description"], "inputSchema": t["inputSchema"]}
        for t in TOOLS
    ]


# ---- JSON-RPC dispatch ---------------------------------------------------

def handle(req):
    method = req.get("method")
    rid = req.get("id")
    if method == "initialize":
        return {
            "jsonrpc": "2.0",
            "id": rid,
            "result": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
            },
        }
    if method in ("notifications/initialized", "initialized"):
        return None
    if method == "ping":
        return {"jsonrpc": "2.0", "id": rid, "result": {}}
    if method == "tools/list":
        return {"jsonrpc": "2.0", "id": rid, "result": {"tools": _tools_list_payload()}}
    if method == "tools/call":
        params = req.get("params") or {}
        name = params.get("name")
        args = params.get("arguments") or {}
        tool = TOOL_BY_NAME.get(name)
        if tool is None:
            return {
                "jsonrpc": "2.0",
                "id": rid,
                "error": {"code": -32602, "message": "unknown tool: %s" % name},
            }
        try:
            result = tool["handler"](args)
        except Exception as e:  # noqa: BLE001
            result = {"content": [{"type": "text", "text": "shim error: %s" % e}], "isError": True}
        return {"jsonrpc": "2.0", "id": rid, "result": result}
    # unknown method
    if rid is None:
        return None
    return {
        "jsonrpc": "2.0",
        "id": rid,
        "error": {"code": -32601, "message": "method not found: %s" % method},
    }


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            continue
        resp = handle(req)
        if resp is not None:
            sys.stdout.write(json.dumps(resp) + "\n")
            sys.stdout.flush()


if __name__ == "__main__":
    main()

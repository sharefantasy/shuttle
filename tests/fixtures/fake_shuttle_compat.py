#!/usr/bin/env python3
import hashlib
import json
import pathlib
import sys


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def digest(value):
    return "sha256:" + hashlib.sha256(canonical(value)).hexdigest()


def footprint():
    identity = {
        "repo_id": "repo:shuttle", "base_oid": "a" * 40, "language": "python",
        "normalized_path": "src/a.py", "name": "alpha", "kind": "function", "line": 1,
        "signature_digest": "sha256:" + "b" * 64,
    }
    symbol = {"symbol_id": digest(identity), **identity}
    return {
        "schema_version": "shuttle_symbol_footprint/v1",
        "basis": {
            "repositories": [{"repo_id": "repo:shuttle", "base_oid": "a" * 40}],
            "symbol_index_ref": "artifact://symbol-index", "symbol_index_digest": "sha256:" + "c" * 64,
            "analyzer_version": "shuttle-symbols/1", "coverage": "exact", "freshness": "fresh", "truncated": False,
        },
        "tasks": [{
            "task_id": "task-a",
            "planned": {"evidence_source": "planner_declared_validated", "declared_read_symbols": [], "declared_write_symbols": [symbol], "unresolved": []},
            "observed": {"evidence_source": "not_observed", "observed_write_symbols": [], "unresolved": []},
            "grants_path_authority": False,
        }],
    }


args = sys.argv[1:]
if args and args[0] == "context-pack":
    value = footprint()
    if "--task-id" in args and args[args.index("--task-id") + 1] == "old-wire":
        value["basis"].pop("freshness")
        task = value["tasks"][0]
        task.update(task.pop("planned"))
        task.pop("observed")
    print(json.dumps({"summary": "ok", "symbol_footprint": value}))
elif args and args[0] == "goal":
    if "--symbol-footprint" in args:
        footprint_path = pathlib.Path(args[args.index("--symbol-footprint") + 1])
        value = json.loads(footprint_path.read_text())
        footprint_digest = digest(value)
        output = {
            "schema_version": "shuttle_goal_parallel_advice/v2", "policy_version": "shuttle_hybrid_dag_policy/v1",
            "planning_basis": {"planning_ref": "planning://g10", "planning_revision": "7", "planning_structure_digest": "sha256:" + "d" * 64},
            "source_intent": {"ref": "artifact://sha256/" + "e" * 64, "digest": "sha256:" + "e" * 64},
            "repository_bases": [{"repo_id": "repo:shuttle", "base_oid": "a" * 40}],
            "symbol_footprint": {"ref": "artifact://sha256/" + footprint_digest[7:], "digest": footprint_digest},
            "symbol_index_bindings": [{"repo_id": "repo:shuttle", "index_digest": "sha256:" + "c" * 64}],
            "model_edges": [], "trusted_addition_candidates": [], "weak_hints": [], "conflicts": [], "proposed_hard_edges": [],
            "schedule": {"batches": [{"index": 0, "tasks": ["task-a"]}], "max_width": 1, "degraded": False},
            "degraded": {"value": False, "reasons": []},
        }
    else:
        output = {
            "schema_version": "shuttle_goal_parallel_advice/v1",
            "source_intent_ref": "artifact://intent",
            "source_intent_digest": "sha256:" + "e" * 64,
            "graph_basis": {"base_oid": "a" * 40, "symbol_index_digest": "sha256:" + "c" * 64, "correction_digest": "sha256:" + "d" * 64, "footprint_source": "planner_guess"},
            "batches": [{"index": 0, "task_ids": ["task-a"]}],
            "serialized": [], "degraded": False, "max_width": 1,
        }
    pathlib.Path(args[args.index("--advice-out") + 1]).write_text(json.dumps(output))
    print("{}")
else:
    raise SystemExit(2)

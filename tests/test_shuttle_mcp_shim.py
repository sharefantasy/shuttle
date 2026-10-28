import importlib.util
import json
import os
import pathlib
import tempfile
import unittest


REPO_ROOT = pathlib.Path(__file__).resolve().parents[1]
MODULE_PATH = REPO_ROOT / "extensions" / "shuttle_mcp_shim.py"


def load_module():
    spec = importlib.util.spec_from_file_location("shuttle_mcp_shim", MODULE_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


shuttle_mcp_shim = load_module()


class EaMcpShimTests(unittest.TestCase):
    def _symbol(self, *, repo_id="repo:shuttle", base_oid=None, path="src/a.py", name="alpha"):
        base_oid = base_oid or "a" * 40
        identity = {
            "repo_id": repo_id,
            "base_oid": base_oid,
            "language": "python",
            "normalized_path": path,
            "name": name,
            "kind": "function",
            "line": 1,
            "signature_digest": "sha256:" + "b" * 64,
        }
        return {"symbol_id": shuttle_mcp_shim._canonical_digest(identity), **identity}

    def _footprint(self, source="planner_declared_validated"):
        symbols = [] if source == "planner_guess" else [self._symbol()]
        unresolved = (
            [{"declaration": "alpha@src/a.py", "reason": "invalid_declaration"}]
            if source == "planner_guess"
            else []
        )
        return {
            "schema_version": "shuttle_symbol_footprint/v1",
            "basis": {
                "repositories": [{"repo_id": "repo:shuttle", "base_oid": "a" * 40}],
                "symbol_index_ref": "artifact://symbol-index",
                "symbol_index_digest": "sha256:" + "c" * 64,
                "analyzer_version": "shuttle-symbols/1",
                "coverage": "exact",
                "freshness": "fresh",
                "truncated": False,
            },
            "tasks": [{
                "task_id": "task-a",
                "planned": {
                    "evidence_source": source,
                    "declared_read_symbols": [],
                    "declared_write_symbols": symbols,
                    "unresolved": unresolved,
                },
                "observed": {
                    "evidence_source": "not_observed",
                    "observed_write_symbols": [],
                    "unresolved": [],
                },
                "grants_path_authority": False,
            }],
        }

    def _v2_advice(self, footprint):
        footprint_digest = shuttle_mcp_shim._canonical_digest(footprint)
        return {
            "schema_version": "shuttle_goal_parallel_advice/v2",
            "policy_version": "shuttle_hybrid_dag_policy/v1",
            "planning_basis": {
                "planning_ref": "planning://g10",
                "planning_revision": "7",
                "planning_structure_digest": "sha256:" + "d" * 64,
            },
            "source_intent": {
                "ref": "artifact://sha256/" + "e" * 64,
                "digest": "sha256:" + "e" * 64,
            },
            "repository_bases": [{"repo_id": "repo:shuttle", "base_oid": "a" * 40}],
            "symbol_footprint": {
                "ref": "artifact://sha256/" + footprint_digest.removeprefix("sha256:"),
                "digest": footprint_digest,
            },
            "symbol_index_bindings": [{"repo_id": "repo:shuttle", "index_digest": "sha256:" + "c" * 64}],
            "model_edges": [],
            "trusted_addition_candidates": [],
            "weak_hints": [],
            "conflicts": [],
            "proposed_hard_edges": [],
            "schedule": {"batches": [{"index": 0, "tasks": ["task-a"]}], "max_width": 1, "degraded": False},
            "degraded": {"value": False, "reasons": []},
        }

    def _v1_advice(self):
        return {
            "schema_version": "shuttle_goal_parallel_advice/v1",
            "source_intent_ref": "artifact://intent",
            "source_intent_digest": "sha256:" + "e" * 64,
            "graph_basis": {
                "base_oid": "a" * 40,
                "symbol_index_digest": "sha256:" + "c" * 64,
                "correction_digest": "sha256:" + "d" * 64,
                "footprint_source": "planner_guess",
            },
            "batches": [{"index": 0, "task_ids": ["task-a"]}],
            "serialized": [],
            "degraded": False,
            "max_width": 1,
        }

    def test_normalize_editplan_ops_replace_variant(self):
        root = "/tmp/probe"
        plan = {
            "kind": "EditPlan",
            "ops": [
                {"type": "replace", "file": "target.txt", "old": "ALPHA", "new": "BETA"}
            ],
        }

        normalized = shuttle_mcp_shim._normalize_edit_plan(plan, root)

        self.assertEqual(
            normalized,
            {
                "version": 1,
                "ops": [
                    {
                        "kind": "search_replace",
                        "path": "target.txt",
                        "search": "ALPHA",
                        "replace": "BETA",
                    }
                ],
            },
        )

    def test_normalize_file_scoped_edits_and_root_relative_paths(self):
        root = "/tmp/workspace"
        plan = {
            "file": f"{root}/nested/target.txt",
            "edits": [{"old_text": "A", "new_text": "B", "line": 3}],
            "intent": "rename placeholder",
        }

        normalized = shuttle_mcp_shim._normalize_edit_plan(plan, root)

        self.assertEqual(normalized["version"], 1)
        self.assertEqual(normalized["intent"], "rename placeholder")
        self.assertEqual(
            normalized["ops"],
            [
                {
                    "kind": "search_replace",
                    "path": "nested/target.txt",
                    "search": "A",
                    "replace": "B",
                    "line_hint": 3,
                }
            ],
        )

    def test_normalize_edits_operations_replace_variant(self):
        root = "/tmp/workspace"
        plan = {
            "version": 1,
            "edits": [
                {
                    "path": "target.txt",
                    "operations": [{"op": "replace", "old": "ALPHA", "new": "BETA"}],
                }
            ],
        }

        normalized = shuttle_mcp_shim._normalize_edit_plan(plan, root)

        self.assertEqual(
            normalized,
            {
                "version": 1,
                "ops": [
                    {
                        "kind": "search_replace",
                        "path": "target.txt",
                        "search": "ALPHA",
                        "replace": "BETA",
                    }
                ],
            },
        )

    def test_canonical_symbol_footprint_round_trips_as_typed_object(self):
        footprint = self._footprint()
        self.assertIs(shuttle_mcp_shim._validate_symbol_footprint(footprint), footprint)
        self.assertEqual(json.loads(shuttle_mcp_shim._canonical_json_bytes(footprint)), footprint)

    def test_old_context_pack_shape_is_rejected_without_translation(self):
        footprint = self._footprint()
        task = footprint["tasks"][0]
        old = {
            "schema_version": footprint["schema_version"],
            "basis": {k: v for k, v in footprint["basis"].items() if k != "freshness"},
            "tasks": [{
                "task_id": task["task_id"],
                "declared_read_symbols": [],
                "declared_write_symbols": [{
                    **task["planned"]["declared_write_symbols"][0],
                    "path": "src/a.py",
                    "definition_anchor": {"line": 1, "signature_digest": "sha256:" + "b" * 64},
                }],
                "observed_write_symbols": [],
                "unresolved": [],
                "evidence_source": "planner_declared_validated",
                "grants_path_authority": False,
            }],
        }
        with self.assertRaisesRegex(ValueError, "freshness|unknown fields"):
            shuttle_mcp_shim._validate_symbol_footprint(old)

    def test_planner_guess_cannot_smuggle_exact_symbols(self):
        footprint = self._footprint("planner_guess")
        footprint["tasks"][0]["planned"]["declared_write_symbols"] = [self._symbol()]
        with self.assertRaisesRegex(ValueError, "planner_guess must not contain exact symbols"):
            shuttle_mcp_shim._validate_symbol_footprint(footprint)

    def test_unknown_schema_and_authority_fields_fail_closed(self):
        footprint = self._footprint()
        footprint["schema_version"] = "shuttle_symbol_footprint/v99"
        with self.assertRaisesRegex(ValueError, "unsupported symbol footprint"):
            shuttle_mcp_shim._validate_symbol_footprint(footprint)
        advice = self._v2_advice(self._footprint())
        advice["workspace"] = "/tmp/not-authority"
        with self.assertRaisesRegex(ValueError, "unknown fields|authority"):
            shuttle_mcp_shim._validate_hybrid_advice(advice)

    def test_hybrid_advice_requires_input_footprint_digest(self):
        footprint = self._footprint()
        advice = self._v2_advice(footprint)
        shuttle_mcp_shim._validate_hybrid_advice(advice, shuttle_mcp_shim._canonical_digest(footprint))
        with self.assertRaisesRegex(ValueError, "does not match"):
            shuttle_mcp_shim._validate_hybrid_advice(advice, "sha256:" + "f" * 64)

    def test_context_pack_typed_output_is_structured_and_legacy_is_guess(self):
        footprint = self._footprint()
        output = json.dumps({"summary": "ok", "symbol_footprint": footprint})
        calls = []

        def fake_run(argv, stdin_text=None, root=None):
            calls.append(list(argv))
            return (True, output, "", 0)

        old_run = shuttle_mcp_shim._run
        try:
            shuttle_mcp_shim._run = fake_run
            result = shuttle_mcp_shim.tool_shuttle_context_pack({"write_symbols": ["alpha@src/a.py"]})
        finally:
            shuttle_mcp_shim._run = old_run
        self.assertFalse(result.get("isError", False), result)
        self.assertEqual(result["details"]["symbol_footprint"], footprint)
        self.assertEqual(result["details"]["legacy_declaration_source"], "planner_guess")
        self.assertEqual(calls[0][calls[0].index("--write-symbol") + 1], "alpha@src/a.py")

    def test_goal_v2_uses_temp_files_validates_and_cleans(self):
        footprint = self._footprint()
        advice = self._v2_advice(footprint)
        seen = {}

        def fake_run(argv, stdin_text=None, root=None):
            seen["argv"] = list(argv)
            footprint_path = pathlib.Path(argv[argv.index("--symbol-footprint") + 1])
            seen["footprint"] = json.loads(footprint_path.read_text())
            advice_path = pathlib.Path(argv[argv.index("--advice-out") + 1])
            seen["temp_dir"] = advice_path.parent
            advice_path.write_text(json.dumps(advice), encoding="utf-8")
            return (True, "{}", "", 0)

        old_run = shuttle_mcp_shim._run
        try:
            shuttle_mcp_shim._run = fake_run
            result = shuttle_mcp_shim.tool_shuttle_goal_parallel_advice({
                "plan": "intent.json",
                "advice_version": "v2",
                "symbol_footprint": footprint,
            })
        finally:
            shuttle_mcp_shim._run = old_run
        self.assertFalse(result.get("isError", False), result)
        self.assertEqual(result["details"]["advice"], advice)
        self.assertEqual(seen["footprint"], footprint)
        self.assertIn("--advice-version", seen["argv"])
        self.assertFalse(seen["temp_dir"].exists())

    def test_goal_auto_selects_v2_when_footprint_is_present(self):
        footprint = self._footprint()
        advice = self._v2_advice(footprint)
        seen = {}

        def fake_run(argv, stdin_text=None, root=None):
            seen["argv"] = list(argv)
            footprint_path = pathlib.Path(argv[argv.index("--symbol-footprint") + 1])
            self.assertEqual(json.loads(footprint_path.read_text()), footprint)
            pathlib.Path(argv[argv.index("--advice-out") + 1]).write_text(json.dumps(advice))
            return (True, "{}", "", 0)

        old_run = shuttle_mcp_shim._run
        try:
            shuttle_mcp_shim._run = fake_run
            result = shuttle_mcp_shim.tool_shuttle_goal_parallel_advice({
                "plan": "intent.json",
                "symbol_footprint": footprint,
            })
        finally:
            shuttle_mcp_shim._run = old_run
        self.assertFalse(result.get("isError", False), result)
        self.assertNotIn("--advice-version", seen["argv"])
        self.assertEqual(result["details"]["selected_advice_version"], "v2")

    def test_goal_auto_falls_back_to_v1_when_footprint_is_absent(self):
        advice = self._v1_advice()
        seen = {}

        def fake_run(argv, stdin_text=None, root=None):
            seen["argv"] = list(argv)
            pathlib.Path(argv[argv.index("--advice-out") + 1]).write_text(json.dumps(advice))
            return (True, "{}", "", 0)

        old_run = shuttle_mcp_shim._run
        try:
            shuttle_mcp_shim._run = fake_run
            result = shuttle_mcp_shim.tool_shuttle_goal_parallel_advice({"plan": "intent.json"})
        finally:
            shuttle_mcp_shim._run = old_run
        self.assertFalse(result.get("isError", False), result)
        self.assertNotIn("--advice-version", seen["argv"])
        self.assertNotIn("--symbol-footprint", seen["argv"])
        self.assertEqual(result["details"]["selected_advice_version"], "v1")

    def test_goal_v2_cleans_temp_files_when_cli_fails(self):
        seen = {}

        def fake_run(argv, stdin_text=None, root=None):
            seen["temp_dir"] = pathlib.Path(argv[argv.index("--advice-out") + 1]).parent
            return (False, "", "failed", 2)

        old_run = shuttle_mcp_shim._run
        try:
            shuttle_mcp_shim._run = fake_run
            result = shuttle_mcp_shim.tool_shuttle_goal_parallel_advice({"plan": "intent.json", "symbol_footprint": self._footprint()})
        finally:
            shuttle_mcp_shim._run = old_run
        self.assertTrue(result.get("isError"))
        self.assertFalse(seen["temp_dir"].exists())

    def test_goal_v1_is_explicit_planner_guess_compatibility(self):
        advice = self._v1_advice()

        def fake_run(argv, stdin_text=None, root=None):
            self.assertEqual(argv[argv.index("--advice-version") + 1], "v1")
            pathlib.Path(argv[argv.index("--advice-out") + 1]).write_text(json.dumps(advice))
            return (True, "{}", "", 0)

        old_run = shuttle_mcp_shim._run
        try:
            shuttle_mcp_shim._run = fake_run
            result = shuttle_mcp_shim.tool_shuttle_goal_parallel_advice({"plan": "intent.json", "advice_version": "v1"})
        finally:
            shuttle_mcp_shim._run = old_run
        self.assertFalse(result.get("isError", False), result)
        advice["graph_basis"]["footprint_source"] = "planner_declared_validated"
        with self.assertRaisesRegex(ValueError, "must remain planner_guess"):
            shuttle_mcp_shim._validate_v1_advice(advice)

    def test_apply_recipe_from_file_builds_from_file_argv(self):
        # from_file must become `--from-file <path>` and reach the dry-run
        # (Phase 1) invocation; we stub _run to capture argv and short-circuit.
        calls = []

        def fake_run(argv):
            calls.append(list(argv))
            # Return a non-ok dry-run so we stop after Phase 1 (no real binary).
            return (False, "", "stopped for test", 1)

        old_run = shuttle_mcp_shim._run
        try:
            shuttle_mcp_shim._run = fake_run
            result = shuttle_mcp_shim.tool_apply_recipe(
                {
                    "from_file": "custom.recipe.json",
                    "file": "api.py",
                    "set": {"func": "f", "removed_param": "unused"},
                }
            )
        finally:
            shuttle_mcp_shim._run = old_run

        self.assertTrue(result.get("isError"))
        self.assertEqual(len(calls), 1, calls)
        argv = calls[0]
        self.assertIn("--from-file", argv)
        self.assertEqual(argv[argv.index("--from-file") + 1], "custom.recipe.json")
        self.assertIn("--file", argv)
        self.assertNotIn("--write", argv)  # dry-run first
        # both --set pairs threaded through
        self.assertEqual(argv.count("--set"), 2)

    def test_apply_recipe_rejects_name_and_from_file_together(self):
        result = shuttle_mcp_shim.tool_apply_recipe(
            {"name": "perf.x", "from_file": "r.json", "file": "api.py"}
        )
        self.assertTrue(result.get("isError"))
        self.assertIn("EITHER", result["content"][0]["text"])

    def test_apply_recipe_requires_name_or_from_file(self):
        result = shuttle_mcp_shim.tool_apply_recipe({"file": "api.py"})
        self.assertTrue(result.get("isError"))
        self.assertIn("from_file", result["content"][0]["text"])

    def test_bare_ea_hint_defers_to_env_bin(self):
        old = os.environ.get("SHUTTLE_BIN")
        try:
            os.environ["SHUTTLE_BIN"] = "/tmp/repo-local-shuttle"
            self.assertEqual(shuttle_mcp_shim.shuttle_bin("shuttle"), "/tmp/repo-local-shuttle")
        finally:
            if old is None:
                os.environ.pop("SHUTTLE_BIN", None)
            else:
                os.environ["SHUTTLE_BIN"] = old

    def test_missing_explicit_hint_defers_to_env_bin(self):
        old = os.environ.get("SHUTTLE_BIN")
        try:
            os.environ["SHUTTLE_BIN"] = "/tmp/repo-local-shuttle"
            self.assertEqual(
                shuttle_mcp_shim.shuttle_bin("/definitely/missing/shuttle"),
                "/tmp/repo-local-shuttle",
            )
        finally:
            if old is None:
                os.environ.pop("SHUTTLE_BIN", None)
            else:
                os.environ["SHUTTLE_BIN"] = old


    # ---- P1.3: ToolOutcome envelope awareness ----------------------------

    def _green(self):
        return json.dumps({
            "version": 1, "tool": "nav", "state": "green",
            "reason_code": "ok", "detail": {"kind": "none"},
        })

    def _blocked(self):
        return json.dumps({
            "version": 1, "tool": "refactor", "state": "blocked",
            "reason_code": "needs-force",
            "next_action": "re-run with --force",
            "force_hint": "--force",
            "whats_missing": ["confidence below High"],
            "detail": {"kind": "none"},
        })

    def _error(self):
        return json.dumps({
            "version": 1, "tool": "recipe", "state": "error",
            "reason_code": "parse", "summary": "cannot index root file",
            "detail": {"kind": "none"},
        })

    def test_parse_outcome_identifies_envelope(self):
        obj = shuttle_mcp_shim._parse_outcome(self._green())
        self.assertIsNotNone(obj)
        self.assertEqual(obj["state"], "green")

    def test_parse_outcome_reads_json_tail_after_prose(self):
        out = "human readable line\n" + self._blocked()
        obj = shuttle_mcp_shim._parse_outcome(out)
        self.assertIsNotNone(obj)
        self.assertEqual(obj["reason_code"], "needs-force")

    def test_parse_outcome_ignores_non_envelope_json(self):
        self.assertIsNone(shuttle_mcp_shim._parse_outcome('{"foo": 1}'))
        self.assertIsNone(shuttle_mcp_shim._parse_outcome("not json at all"))
        self.assertIsNone(shuttle_mcp_shim._parse_outcome(""))

    def test_finalize_green_is_not_error(self):
        r = shuttle_mcp_shim._finalize(True, self._green(), "", 0)
        self.assertNotIn("isError", r)
        self.assertIn("green", r["content"][0]["text"])

    def test_finalize_blocked_is_not_error(self):
        # A blocked outcome is one-step-fixable, NOT a failure — even though a
        # tool emitting it exits non-zero, the envelope state wins.
        r = shuttle_mcp_shim._finalize(False, self._blocked(), "", 3)
        self.assertNotIn("isError", r)
        self.assertIn("needs-force", r["content"][0]["text"])

    def test_finalize_error_is_iserror(self):
        r = shuttle_mcp_shim._finalize(False, self._error(), "", 1)
        self.assertTrue(r.get("isError"))

    def test_finalize_launch_failure_is_iserror(self):
        r = shuttle_mcp_shim._finalize(False, "", "boom", -1)
        self.assertTrue(r.get("isError"))

    def test_finalize_falls_back_to_exit_code_without_envelope(self):
        # No envelope on stdout: preserve legacy exit-code semantics.
        ok = shuttle_mcp_shim._finalize(True, "plain ok output", "", 0)
        self.assertNotIn("isError", ok)
        bad = shuttle_mcp_shim._finalize(False, "plain output", "stderr detail", 2)
        self.assertTrue(bad.get("isError"))

    def test_recipe_low_confidence_block_is_not_error(self):
        # Low-confidence recipe block is resubmit-with-force, so NOT isError.
        def fake_run(argv):
            return (False, "", "batch gate: step is low-confidence", 1)

        old_run = shuttle_mcp_shim._run
        try:
            shuttle_mcp_shim._run = fake_run
            result = shuttle_mcp_shim.tool_apply_recipe(
                {"name": "perf.x", "file": "api.py"}
            )
        finally:
            shuttle_mcp_shim._run = old_run
        self.assertNotIn("isError", result)
        self.assertIn("force:true", result["content"][0]["text"])

    def test_recipe_genuine_rejection_is_error(self):
        # A non-low-confidence rejection stays a hard error.
        def fake_run(argv):
            return (False, "", "unknown param 'zzz'", 1)

        old_run = shuttle_mcp_shim._run
        try:
            shuttle_mcp_shim._run = fake_run
            result = shuttle_mcp_shim.tool_apply_recipe(
                {"name": "perf.x", "file": "api.py"}
            )
        finally:
            shuttle_mcp_shim._run = old_run
        self.assertTrue(result.get("isError"))

    # ---- ui_* host-surface wiring (design §9: seven evidence verbs) ---------

    UI_VERBS = ("verify", "couple", "render", "adopt", "assess", "canvas", "receipt")

    def test_all_seven_ui_verbs_are_registered(self):
        # Design doc §9 bolds `receipt` alongside verify/adopt/couple/render/
        # assess/canvas as an exposed pi/MCP exit. The MCP registry must carry
        # exactly the seven ui_ tools the CLI ships — no more, no fewer.
        names = {t["name"] for t in shuttle_mcp_shim.TOOLS}
        for verb in self.UI_VERBS:
            self.assertIn(f"ui_{verb}", names, f"ui_{verb} must be registered")
        ui_names = sorted(n for n in names if n.startswith("ui_"))
        self.assertEqual(ui_names, sorted(f"ui_{v}" for v in self.UI_VERBS))

    def test_ui_receipt_is_wired_to_the_receipt_verb(self):
        # The handler must dispatch to `shuttle ui receipt` (not some other verb),
        # and its closed inputSchema must require the `request` object only.
        entry = shuttle_mcp_shim.TOOL_BY_NAME["ui_receipt"]
        self.assertIs(entry["handler"], shuttle_mcp_shim.tool_ui_receipt)
        captured = {}

        def fake_run(args, stdin_text=None, root=None):
            captured["args"] = args
            captured["stdin"] = stdin_text
            return (True, '{"schema_version":"pde_campaign_phase_receipt/v1"}', "", 0)

        old_run = shuttle_mcp_shim._run
        try:
            shuttle_mcp_shim._run = fake_run
            result = shuttle_mcp_shim.tool_ui_receipt({"request": {"campaign_id": "c"}})
        finally:
            shuttle_mcp_shim._run = old_run
        self.assertFalse(result.get("isError"))
        self.assertEqual(captured["args"][-2:], ["ui", "receipt"])
        self.assertEqual(json.loads(captured["stdin"]), {"campaign_id": "c"})
        schema = entry["inputSchema"]
        self.assertFalse(schema["additionalProperties"])
        self.assertEqual(schema["required"], ["request"])

    def test_ui_receipt_missing_request_fails_closed(self):
        result = shuttle_mcp_shim.tool_ui_receipt({})
        self.assertTrue(result.get("isError"))

    def _ea_bin_or_skip(self):
        candidate = REPO_ROOT / ".shuttletmp" / "target" / "debug" / "shuttle"
        if not candidate.exists():
            self.skipTest("built shuttle binary not present (.shuttletmp/target/debug/shuttle)")
        return str(candidate)

    def test_ui_receipt_seals_through_shim_end_to_end(self):
        # Real E2E across the wired host surface: drive ui_adopt then ui_receipt
        # through the MCP shim handlers against the built shuttle binary, proving the
        # seventh verb transports a fail-closed sealed receipt verbatim.
        shuttle = self._ea_bin_or_skip()
        digest = "sha256:" + ("a" * 64)
        order_atom = {
            "family": "order", "relation": "before", "args": ["a", "b"],
            "axes": [{"name": "x", "kind": "metric"}], "allen": "before",
            "frame": "viewport", "time_quantifier": "at",
        }
        adopt_req = {
            "surface_id": "screen:checkout",
            "artifact_digest": digest,
            "frame": {"tolerance": 0.5, "boxes": [
                {"element": "a", "axis": "x", "start": 0.0, "end": 10.0},
                {"element": "b", "axis": "x", "start": 20.0, "end": 30.0},
            ]},
            "atoms": [order_atom],
            "principles": [{"name": "Ordered", "formula": {"op": "atom", "atom": 0},
                            "is_geometric_proxy": False}],
            "thresholds": {},
        }
        adopt = shuttle_mcp_shim.tool_ui_adopt({"request": adopt_req, "shuttle_bin": shuttle})
        self.assertFalse(adopt.get("isError"), adopt)
        adopt_text = "".join(
            c["text"] for c in adopt.get("content", []) if c.get("type") == "text"
        )
        adopt_resp = json.loads(adopt_text)
        self.assertEqual(adopt_resp.get("outcome"), "satisfied")
        projection = adopt_resp["projection"]

        receipt_req = {
            "campaign_id": "campaign-checkout-01",
            "expected_campaign_revision": 3,
            "idempotency_key": "idem-ui-receipt-shim-e2e-1",
            "action": "projection.ready",
            "from_phase": "WAITING_EVIDENCE",
            "to_phase": "EVIDENCE_COMMITTED",
            "owner": "shuttle-ui",
            "authority_artifact": None,
            "effect": {"state": "none", "receipt_ref": None, "receipt_digest": None},
            "decision": None,
            "observed_at": "2026-09-15T08:30:00Z",
            "projection": projection,
        }
        sealed = shuttle_mcp_shim.tool_ui_receipt({"request": receipt_req, "shuttle_bin": shuttle})
        self.assertFalse(sealed.get("isError"), sealed)
        sealed_text = "".join(
            c["text"] for c in sealed.get("content", []) if c.get("type") == "text"
        )
        receipt = json.loads(sealed_text.strip().splitlines()[-1])
        self.assertEqual(receipt.get("schema_version"), "pde_campaign_phase_receipt/v1")
        self.assertEqual(receipt["owner_artifact"]["ref"],
                         projection["projection_id"])
        self.assertEqual(receipt["owner_artifact"]["type"], "ui_projection/v1")

    def test_ui_receipt_refuses_decided_effect_on_blind_spot_e2e(self):
        # 「绝不瞎猜」across the surface: a decided (applied) effect on a
        # non-satisfied projection must fail closed at the shim boundary.
        shuttle = self._ea_bin_or_skip()
        digest = "sha256:" + ("c" * 64)
        order_atom = {
            "family": "order", "relation": "before", "args": ["a", "b"],
            "axes": [{"name": "x", "kind": "metric"}], "allen": "before",
            "frame": "viewport", "time_quantifier": "at",
        }
        # Drop element b so the proxy-family order atom cannot decide -> null.
        adopt_req = {
            "surface_id": "screen:checkout",
            "artifact_digest": digest,
            "frame": {"tolerance": 0.5, "boxes": [
                {"element": "a", "axis": "x", "start": 0.0, "end": 10.0},
            ]},
            "atoms": [order_atom],
            "principles": [{"name": "Ordered", "formula": {"op": "atom", "atom": 0},
                            "is_geometric_proxy": False}],
            "thresholds": {},
        }
        adopt = shuttle_mcp_shim.tool_ui_adopt({"request": adopt_req, "shuttle_bin": shuttle})
        self.assertFalse(adopt.get("isError"), adopt)
        adopt_text = "".join(
            c["text"] for c in adopt.get("content", []) if c.get("type") == "text"
        )
        projection = json.loads(adopt_text)["projection"]

        receipt_req = {
            "campaign_id": "campaign-checkout-01",
            "expected_campaign_revision": 3,
            "idempotency_key": "idem-ui-receipt-shim-blind-1",
            "action": "projection.ready",
            "from_phase": "WAITING_EVIDENCE",
            "to_phase": "EVIDENCE_COMMITTED",
            "owner": "shuttle-ui",
            "authority_artifact": None,
            "effect": {"state": "applied", "receipt_ref": "file:///campaign/effect",
                       "receipt_digest": None},
            "decision": None,
            "observed_at": "2026-09-15T08:30:00Z",
            "projection": projection,
        }
        sealed = shuttle_mcp_shim.tool_ui_receipt({"request": receipt_req, "shuttle_bin": shuttle})
        self.assertTrue(sealed.get("isError"),
                        "decided effect on a blind-spot projection must fail closed")



if __name__ == "__main__":
    unittest.main()

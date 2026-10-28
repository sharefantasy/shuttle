const assert = require("node:assert/strict");
const { createHash } = require("node:crypto");
const { existsSync } = require("node:fs");
const { homedir } = require("node:os");
const path = require("node:path");
const Module = require("node:module");

// shuttle-tools.ts is a pi extension, so its runtime dependencies belong to pi.
// Reuse that already-installed dependency root; never install or mutate deps.
const piModules = path.join(homedir(), ".pi", "agent", "npm", "node_modules");
if (!existsSync(path.join(piModules, "jiti"))) {
  throw new Error(`TS compatibility test preflight closed: missing pi runtime jiti at ${piModules}`);
}
process.env.NODE_PATH = [piModules, process.env.NODE_PATH].filter(Boolean).join(path.delimiter);
Module._initPaths();
const { createJiti } = require("jiti");

function canonicalJson(value) {
  if (Array.isArray(value)) return "[" + value.map(canonicalJson).join(",") + "]";
  if (value && typeof value === "object") {
    return "{" + Object.keys(value).sort().map((key) => JSON.stringify(key) + ":" + canonicalJson(value[key])).join(",") + "}";
  }
  return JSON.stringify(value);
}

function digest(value) {
  return "sha256:" + createHash("sha256").update(canonicalJson(value)).digest("hex");
}

function footprint(source = "planner_declared_validated") {
  const identity = {
    repo_id: "repo:shuttle", base_oid: "a".repeat(40), language: "python",
    normalized_path: "src/a.py", name: "alpha", kind: "function", line: 1,
    signature_digest: "sha256:" + "b".repeat(64),
  };
  const symbol = { symbol_id: digest(identity), ...identity };
  return {
    schema_version: "shuttle_symbol_footprint/v1",
    basis: {
      repositories: [{ repo_id: "repo:shuttle", base_oid: "a".repeat(40) }],
      symbol_index_ref: "artifact://symbol-index", symbol_index_digest: "sha256:" + "c".repeat(64),
      analyzer_version: "shuttle-symbols/1", coverage: "exact", freshness: "fresh", truncated: false,
    },
    tasks: [{
      task_id: "task-a",
      planned: {
        evidence_source: source, declared_read_symbols: [],
        declared_write_symbols: source === "planner_guess" ? [] : [symbol],
        unresolved: source === "planner_guess" ? [{ declaration: "alpha@src/a.py", reason: "invalid_declaration" }] : [],
      },
      observed: { evidence_source: "not_observed", observed_write_symbols: [], unresolved: [] },
      grants_path_authority: false,
    }],
  };
}

(async () => {
  const jiti = createJiti(__filename, { interopDefault: true });
  const imported = await jiti.import(path.resolve(__dirname, "../extensions/shuttle-tools.ts"));
  const tools = [];
  (imported.default || imported)({ registerTool: (tool) => tools.push(tool) });
  const byName = Object.fromEntries(tools.map((tool) => [tool.name, tool]));
  assert.ok(byName.shuttle_context_pack);
  assert.ok(byName.shuttle_goal_parallel_advice);

  const canonical = footprint();
  const fakeEa = path.join(__dirname, "fixtures/fake_shuttle_compat.py");
  let result = await byName.shuttle_context_pack.execute("call", {
    shuttle_bin: fakeEa, root: ".", symbol_index: "index.json", write_symbols: ["alpha@src/a.py"],
  });
  assert.equal(result.isError, undefined, JSON.stringify(result));
  assert.deepEqual(result.details.symbol_footprint, canonical);
  assert.equal(result.details.legacy_declaration_source, "planner_guess");
  assert.equal(result.details.grants_path_authority, false);

  result = await byName.shuttle_context_pack.execute("call", { shuttle_bin: fakeEa, root: ".", task_id: "old-wire" });
  assert.equal(result.isError, true);
  assert.match(result.content[0].text, /missing fields|unknown fields/);

  result = await byName.shuttle_goal_parallel_advice.execute("call", {
    shuttle_bin: fakeEa, root: ".", plan: "intent.json", symbol_footprint: canonical,
  });
  assert.equal(result.isError, undefined, JSON.stringify(result));
  assert.equal(result.details.advice.schema_version, "shuttle_goal_parallel_advice/v2");
  assert.equal(result.details.selected_advice_version, "v2");
  assert.equal(result.details.read_only, true);
  assert.equal(result.details.authority, "none");

  result = await byName.shuttle_goal_parallel_advice.execute("call", {
    shuttle_bin: fakeEa, root: ".", plan: "intent.json",
  });
  assert.equal(result.isError, undefined, JSON.stringify(result));
  assert.equal(result.details.advice.schema_version, "shuttle_goal_parallel_advice/v1");
  assert.equal(result.details.selected_advice_version, "v1");

  const guessed = footprint("planner_guess");
  guessed.tasks[0].planned.declared_write_symbols = canonical.tasks[0].planned.declared_write_symbols;
  result = await byName.shuttle_goal_parallel_advice.execute("call", { plan: "intent.json", symbol_footprint: guessed });
  assert.equal(result.isError, true);
  assert.match(result.content[0].text, /planner_guess must not contain exact symbols/);

  console.log("shuttle-tools compatibility tests: PASS");
})().catch((error) => { console.error(error); process.exit(1); });

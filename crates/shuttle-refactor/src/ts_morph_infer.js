"use strict";
// One-shot TypeScript type-inference bridge for shuttle-refactor (docs §2.2 / §5,
// batch 3). NON-DAEMON: parse a single source in-memory, print the concrete
// type of each requested local, then exit. shuttle only pulls this in when built
// with the optional `ts-morph` feature AND locals+ctags left a TS annotation
// unresolved (§7: fill genuine placeholders only, never guess).
//
// Protocol: stdin = JSON {source, fileName, names:[...]}.
//   stdout = JSON {name: "TypeText"} for the subset with a concrete, useful
//            type. Exit 3 = ts-morph not installed (caller keeps the
//            placeholder); exit 4 = bad input.
const fs = require("fs");

let tsm;
try {
  tsm = require("ts-morph");
} catch (e) {
  process.exit(3); // ts-morph unavailable -> caller keeps TODO_TYPE
}
const { Project, SyntaxKind } = tsm;

let payload;
try {
  payload = JSON.parse(fs.readFileSync(0, "utf8")); // fd 0 = stdin
} catch (e) {
  process.exit(4);
}

const project = new Project({
  useInMemoryFileSystem: true,
  compilerOptions: { allowJs: true, noEmit: true, strict: false },
});
const sf = project.createSourceFile(
  payload.fileName || "shuttle_infer.ts",
  String(payload.source || "")
);

const wanted = new Set(payload.names || []);
// §7 honesty: a non-informative inferred type is NOT an answer -> skip it so
// the caller keeps the visible `TODO_TYPE` placeholder instead of a fake type.
const useless = new Set(["any", "unknown", "never", "null", "undefined", "{}", "object"]);
const out = {};

for (const d of sf.getDescendantsOfKind(SyntaxKind.VariableDeclaration)) {
  const name = d.getName();
  if (!wanted.has(name) || out[name] !== undefined) continue;
  let text;
  try {
    text = d.getType().getText(d);
  } catch (e) {
    continue;
  }
  if (!text) continue;
  text = text.trim();
  if (useless.has(text)) continue;
  if (text.includes("import(")) continue; // unresolved cross-module reference
  out[name] = text;
}

process.stdout.write(JSON.stringify(out));

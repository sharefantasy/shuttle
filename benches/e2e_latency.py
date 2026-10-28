#!/usr/bin/env python3
"""End-to-end latency benchmark for the `shuttle` execution kernel (perf plan §4, 三批之三).

Purpose
-------
The perf plan lands five optimizations (①..⑤). Four of them (②③④ + ①'s cache)
collapse *process spawns* or reuse a scan whose win is structural and known at
edit time. The one genuinely data-dependent question the plan calls out is:

    §4.7 "补 benches/ 端到端延迟基准,对比每项前后的 wall_ms / spawn 次数 /
          ctags 扫描字节数,用数据决定 ⑤ 是否值得。"

So this harness measures the two things that are *not* obvious from the diff:

  1. ⑤ scan-scope  — the real wall_ms of `shuttle repomap --focus F` with vs without
     `--scan-scope`, plus the ctags input corpus each mode crawls (bytes/files).
     This is the "is ⑤ worth it" number.
  2. spawn counts  — the ②③④ collapses, reported as a reference table (these are
     structural: context-pack 10→1, apply gate 2→1, locate batch N→1).

Design notes
------------
* stdlib only, no criterion / no third-party deps — matches the latency-first,
  instant-to-run spirit of `crates/shuttle-editcore/benches/apply_latency.rs`.
* Wall clock via time.perf_counter() (no `time(1)`, which writes stderr → blocked
  in some sandboxes). All scratch stays inside the repo working dir.
* The full-repo scan can be pathological (it crawls untracked bulk like
  `vendor/runs` that ctags does *not* exclude); a timeout guard turns that into a
  data point ("full scan unusable") rather than hanging the bench.

Usage
-----
    python3 benches/e2e_latency.py [--focus PATH] [--budget N] [--timeout S]
                                   [--shuttle PATH] [--reps K]

Exit code is 0 on a completed run (even if the full-scan arm times out — that is
a legitimate measurement, not a harness failure).
"""

import argparse
import json
import os
import subprocess
import sys
import time

# ctags exclude set mirrored from SymbolTable::scan (crates/shuttle-symbols/src/lib.rs).
# Dirs pruned during the corpus walk; glob suffixes skipped file-wise.
EXCLUDE_DIRS = {
    "node_modules", "dist", "build", "out", "target", ".git", ".venv", "vendor",
}
EXCLUDE_SUFFIXES = (".min.js", ".lock", "-lock.json")


def repo_root() -> str:
    """Repo root = parent of the dir holding this script (benches/..)."""
    here = os.path.dirname(os.path.abspath(__file__))
    return os.path.dirname(here)


def find_shuttle(root: str, override: str | None) -> str:
    if override:
        return override
    cand = os.path.join(root, "target", "release", "shuttle")
    if os.path.isfile(cand) and os.access(cand, os.X_OK):
        return cand
    cand_dbg = os.path.join(root, "target", "debug", "shuttle")
    if os.path.isfile(cand_dbg) and os.access(cand_dbg, os.X_OK):
        return cand_dbg
    sys.exit(
        "shuttle binary not found; build it first:\n"
        "    cargo build --release -p shuttle-cli\n"
        "or pass --shuttle <path>."
    )


def corpus(root: str, tracked_only: bool) -> tuple[int, int]:
    """(total_bytes, file_count) of the source corpus ctags would ingest.

    tracked_only=True  → exactly `git ls-files` (the universe --scan-scope draws
                         from; the scoped set is a subset of this).
    tracked_only=False → a `-R root` filesystem walk minus the exclude set (what
                         a bare full scan crawls, *including* untracked bulk).
    """
    if tracked_only:
        out = subprocess.run(
            ["git", "-C", root, "ls-files", "-z"],
            capture_output=True, check=True,
        ).stdout
        rels = [p for p in out.split(b"\0") if p]
        total = 0
        count = 0
        for rel in rels:
            p = os.path.join(root, rel.decode("utf-8", "surrogateescape"))
            try:
                total += os.path.getsize(p)
                count += 1
            except OSError:
                pass
        return total, count

    total = 0
    count = 0
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in EXCLUDE_DIRS]
        for fn in filenames:
            if fn.endswith(EXCLUDE_SUFFIXES):
                continue
            try:
                total += os.path.getsize(os.path.join(dirpath, fn))
                count += 1
            except OSError:
                pass
    return total, count


def run_repomap(shuttle: str, root: str, focus: str, budget: int, scoped: bool,
                timeout: float) -> dict:
    """Time one `shuttle repomap` invocation. Returns wall_ms / entries / status."""
    cmd = [shuttle, "repomap", "--root", root, "--focus", focus,
           "--budget", str(budget)]
    if scoped:
        cmd.append("--scan-scope")
    t0 = time.perf_counter()
    try:
        proc = subprocess.run(cmd, capture_output=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return {"status": f"TIMEOUT(>{timeout:g}s)", "wall_ms": None,
                "entries": None, "exit": None}
    wall_ms = (time.perf_counter() - t0) * 1000.0
    entries = None
    try:
        doc = json.loads(proc.stdout.decode("utf-8", "replace"))
        # repo_map output shape may vary; count the obvious list-ish field.
        if isinstance(doc, dict):
            for k in ("entries", "files", "symbols", "map"):
                v = doc.get(k)
                if isinstance(v, list):
                    entries = len(v)
                    break
            if entries is None:
                entries = sum(len(v) for v in doc.values()
                              if isinstance(v, list)) or None
        elif isinstance(doc, list):
            entries = len(doc)
    except Exception:
        entries = None
    return {"status": "ok" if proc.returncode == 0 else f"exit={proc.returncode}",
            "wall_ms": wall_ms, "entries": entries, "exit": proc.returncode}


def bench_scope(shuttle, root, focus, budget, timeout, reps):
    print(f"\n--- ⑤ scan-scope: repomap --focus {focus} (budget={budget}) ---")

    full_bytes, full_files = corpus(root, tracked_only=False)
    trk_bytes, trk_files = corpus(root, tracked_only=True)

    def best_of(scoped):
        best = None
        for _ in range(reps):
            r = run_repomap(shuttle, root, focus, budget, scoped, timeout)
            if r["wall_ms"] is None:  # timeout → report immediately
                return r
            if best is None or r["wall_ms"] < best["wall_ms"]:
                best = r
        return best

    scoped = best_of(True)
    full = best_of(False)  # may time out; that is the point

    def ms(r):
        return f"{r['wall_ms']:.0f}" if r["wall_ms"] is not None else r["status"]

    def ent(r):
        return str(r["entries"]) if r["entries"] is not None else "-"

    print(f"{'mode':<10}{'wall_ms':>12}{'entries':>10}"
          f"{'corpus_bytes':>16}{'corpus_files':>14}")
    print(f"{'full':<10}{ms(full):>12}{ent(full):>10}"
          f"{full_bytes:>16}{full_files:>14}")
    print(f"{'scoped':<10}{ms(scoped):>12}{ent(scoped):>10}"
          f"{trk_bytes:>16}{trk_files:>14}   (≤ tracked; actual scope ⊆ this)")

    # Headline deltas.
    untracked = full_bytes - trk_bytes
    print(f"\nfull scan crawls {full_bytes/1e6:.1f} MB across {full_files} files; "
          f"scoped draws from a tracked universe of {trk_bytes/1e6:.1f} MB "
          f"({trk_files} files).")
    print(f"⑤ eliminates ~{untracked/1e6:.1f} MB of untracked bulk outright "
          f"(files ctags -R would crawl but git does not track).")
    if scoped["wall_ms"] is not None and full["wall_ms"] is not None:
        sp = full["wall_ms"] / scoped["wall_ms"]
        print(f"wall-clock speedup: {sp:.2f}x "
              f"({full['wall_ms']:.0f}ms → {scoped['wall_ms']:.0f}ms).")
    elif full["wall_ms"] is None:
        print(f"full scan hit the {timeout:g}s timeout → ⑤ turns an "
              f"unusable operation into a {scoped['wall_ms']:.0f}ms one.")


def print_spawn_reference():
    print("\n--- spawn-count reference (structural collapses, ② ③ ④) ---")
    rows = [
        ("context-pack (②)", "10", "1", "one SymbolTable reused for the whole pack"),
        ("apply --gate (③)", "2", "1", "probe + real fused into a single process"),
        ("locate --pattern (④)", "N", "1", "batch idents, salience hit in one spawn"),
    ]
    print(f"{'op':<22}{'before':>8}{'after':>7}   note")
    for op, b, a, note in rows:
        print(f"{op:<22}{b:>8}{a:>7}   {note}")
    print("(spawn counts are edit-time-known from the collapse; the runtime-"
          "variable metric worth benching is ⑤ above.)")


def main():
    ap = argparse.ArgumentParser(description="shuttle end-to-end latency bench")
    ap.add_argument("--focus", default="crates/shuttle-symbols/src/lib.rs")
    ap.add_argument("--budget", type=int, default=512)
    ap.add_argument("--timeout", type=float, default=90.0,
                    help="per-invocation wall timeout (s); full scan may hit it")
    ap.add_argument("--reps", type=int, default=2,
                    help="repetitions per mode; the fastest is reported")
    ap.add_argument("--shuttle", default=None, help="path to the shuttle binary")
    args = ap.parse_args()

    root = repo_root()
    shuttle = find_shuttle(root, args.shuttle)
    try:
        shuttle_size = os.path.getsize(shuttle)
    except OSError:
        shuttle_size = -1

    print("=== shuttle end-to-end latency bench (perf plan §4.7) ===")
    print(f"repo: {root}")
    print(f"shuttle:   {shuttle} ({shuttle_size} bytes)")

    focus_abs = os.path.join(root, args.focus)
    if not os.path.isfile(focus_abs):
        sys.exit(f"--focus not found: {focus_abs}")

    bench_scope(shuttle, root, args.focus, args.budget, args.timeout, args.reps)
    print_spawn_reference()
    print("\ndone.")


if __name__ == "__main__":
    main()

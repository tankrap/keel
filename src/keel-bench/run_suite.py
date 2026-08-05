#!/usr/bin/env python3
"""Single entry point for keel's live-LLM flywheel benchmark suite (NEW-1094).

Runs every benchmark listed in `bench-config.json` under one pinned, reproducible config and
aggregates the results into one report (JSON + markdown), each metric carrying its Wilson 95% CI.

    python3 run_suite.py --dry-run     # free: import every harness + validate scenario tables,
                                       # NO API calls, NO keel binary, NO token required
    python3 run_suite.py               # LIVE: runs both harnesses — COSTS REAL API CREDITS
    python3 run_suite.py --only flywheel-synthetic   # run a single benchmark by id

The pinned trials/workers come from bench-config.json and are exported to the environment BEFORE
the harnesses are imported (they read TRIALS/WORKERS at import). An already-set env var wins, so
you can still `TRIALS=1 python3 run_suite.py` for a quick smoke run.
"""
import argparse
import datetime
import importlib
import json
import os
import pathlib
import sys

HERE = pathlib.Path(__file__).resolve().parent
CONFIG_PATH = HERE / "bench-config.json"
sys.path.insert(0, str(HERE))  # so `import flywheel_bench` / `corpus_bench` resolve

import bench_manifest  # noqa: E402  (after sys.path insert so it resolves)


def load_config():
    return json.loads(CONFIG_PATH.read_text())


def pin_env(cfg):
    """Export the pinned run config so harness modules pick it up at import time. Env wins."""
    os.environ.setdefault("TRIALS", str(cfg["run"]["trials"]))
    os.environ.setdefault("WORKERS", str(cfg["run"]["workers"]))


def validate_benchmark(bench):
    """Import a harness and confirm its scenario table parses at the pinned shape. No API calls."""
    problems = []
    try:
        mod = importlib.import_module(bench["module"])
    except Exception as e:  # import failure is the thing we most want dry-run to catch
        return None, [f"import {bench['module']} failed: {e!r}"]

    scen = getattr(mod, "SCEN", None)
    if not isinstance(scen, (list, tuple)):
        return mod, [f"{bench['module']}.SCEN is missing or not a list"]

    if len(scen) != bench["scenarios"]:
        problems.append(f"scenario count {len(scen)} != pinned {bench['scenarios']}")

    arity = bench["scenario_arity"]
    for i, row in enumerate(scen):
        if not isinstance(row, tuple) or len(row) != arity:
            problems.append(f"scenario[{i}] arity {len(row) if isinstance(row, tuple) else 'n/a'} != {arity}")
            continue
        if not all(isinstance(f, str) and f.strip() for f in row):
            problems.append(f"scenario[{i}] has an empty/non-string field")

    if not callable(getattr(mod, "run", None)):
        problems.append(f"{bench['module']}.run() is missing")
    return mod, problems


def dry_run(cfg):
    print("keel-bench dry-run — importing harnesses + validating scenario tables (no API calls)\n")
    ok = True
    for b in cfg["benchmarks"]:
        _mod, problems = validate_benchmark(b)
        if problems:
            ok = False
            print(f"  ✗ {b['id']:<20} ({b['module']})")
            for p in problems:
                print(f"      - {p}")
        else:
            print(f"  ✓ {b['id']:<20} ({b['module']}): {b['scenarios']} scenarios × arity {b['scenario_arity']} OK")
    print()
    if not ok:
        print("dry-run FAIL — see problems above.")
        return 1
    # Reproducibility: the manifest must be deterministic (identical inputs → identical SHA). Build it
    # twice and confirm, so `--dry-run` verifies the suite is verifiable for free (no API).
    m1 = bench_manifest.build_manifest(cfg)
    m2 = bench_manifest.build_manifest(cfg)
    if m1 != m2:
        print("dry-run FAIL — reproducibility manifest is non-deterministic.")
        return 1
    print(f"dry-run PASS — {len(cfg['benchmarks'])} harness(es) import and parse cleanly.")
    print(f"reproducibility manifest: {m1['manifest_sha256']}")
    print("  (stable SHA over every pinned scenario table + the result-affecting config)")
    return 0


def markdown_report(cfg, summaries):
    ts = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    ref = {b["id"]: b for b in cfg["benchmarks"]}
    out = [f"# keel flywheel benchmark suite — result",
           "",
           f"- suite version: `{cfg['version']}` (schema `{cfg['schema']}`, {cfg['issue']})",
           f"- generated: {ts}",
           f"- solver: `{cfg['models']['solver']}` · judge: `{cfg['models']['judge']}`",
           f"- trials: {cfg['run']['trials']} · workers: {cfg['run']['workers']} · Wilson z: {cfg['run']['wilson_z']}",
           f"- reproducibility manifest: `{bench_manifest.build_manifest(cfg)['manifest_sha256']}`",
           ""]
    for s in summaries:
        b = ref.get(s["id"], {})
        wo, wi = s["without"], s["with"]
        out += [f"## {s['id']}",
                "",
                b.get("measures", ""),
                "",
                f"- retrieval surfaced the lesson: {s['retrieved']}/{s['scenarios']}",
                "",
                "| condition | correct | pct | 95% CI (Wilson) |",
                "|---|---|---|---|",
                f"| WITHOUT keel brief | {wo['correct']}/{wo['total']} | {wo['pct']}% | {wo['ci95_pct'][0]}–{wo['ci95_pct'][1]}% |",
                f"| WITH keel brief | {wi['correct']}/{wi['total']} | {wi['pct']}% | {wi['ci95_pct'][0]}–{wi['ci95_pct'][1]}% |",
                f"| **lift** | **+{s['lift_points']} points** | | |",
                ""]
        r = b.get("reference")
        if r:
            out += [f"Reference bar: WITHOUT {r['without']} → WITH {r['with']} (+{r['lift_points']} points).", ""]
    return "\n".join(out)


def full_run(cfg, only=None):
    pin_env(cfg)
    summaries = []
    for b in cfg["benchmarks"]:
        if only and b["id"] != only:
            continue
        print(f"\n═══ {b['id']} ═══")
        mod = importlib.import_module(b["module"])
        summaries.append(mod.run())

    if not summaries:
        print(f"no benchmark matched --only {only!r}", file=sys.stderr)
        return 1

    report = {
        "schema": cfg["schema"],
        "suite_version": cfg["version"],
        "issue": cfg["issue"],
        "generated_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "models": cfg["models"],
        "run": cfg["run"],
        # Pin the exact inputs this result was produced from: anyone can recompute the manifest and
        # confirm a later run used byte-identical scenarios + config (reproducibility).
        "manifest": bench_manifest.build_manifest(cfg),
        "benchmarks": summaries,
    }
    json_path = HERE / "bench-report.json"
    md_path = HERE / "bench-report.md"
    json_path.write_text(json.dumps(report, indent=2) + "\n")
    md_path.write_text(markdown_report(cfg, summaries))
    print(f"\nwrote {json_path.name} + {md_path.name}")
    return 0


def main():
    ap = argparse.ArgumentParser(description="keel flywheel benchmark suite runner")
    ap.add_argument("--dry-run", action="store_true",
                    help="import harnesses + validate scenario tables; make NO API calls")
    ap.add_argument("--manifest", action="store_true",
                    help="print the reproducibility manifest (SHA over pinned inputs); make NO API calls")
    ap.add_argument("--only", metavar="ID", help="run a single benchmark by its config id")
    args = ap.parse_args()

    cfg = load_config()
    if args.manifest:
        print(json.dumps(bench_manifest.build_manifest(cfg), indent=2))
        return 0
    if args.dry_run:
        return dry_run(cfg)
    return full_run(cfg, only=args.only)


if __name__ == "__main__":
    raise SystemExit(main())

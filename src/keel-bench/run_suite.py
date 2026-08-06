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
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
CONFIG_PATH = HERE / "bench-config.json"
sys.path.insert(0, str(HERE))  # so `import flywheel_bench` / `corpus_bench` resolve

import bench_manifest  # noqa: E402  (after sys.path insert so it resolves)
import bench_compare  # noqa: E402

# The keel binary under test — same resolution as bench_common (env override, else the release build).
KEEL_BIN = os.environ.get("KEEL_BIN", str(pathlib.Path.home() / "keel/src/rust/target/release/keel"))


def keel_version():
    """The keel binary's own build identity ({version, git_commit, dirty}) via `keel native version` —
    provenance tying a result to the exact keel build that produced it. Returns {"error": ...} if the
    binary is missing, too old to know the subcommand, or emits unparseable output. Lives in the runner
    (not bench_common) so it never changes the manifest-hashed harness surface."""
    try:
        r = subprocess.run([KEEL_BIN, "native", "version"], capture_output=True, text=True, timeout=30)
    except FileNotFoundError:
        return {"error": f"keel binary not found at {KEEL_BIN}"}
    except Exception as e:  # timeout / OS error — report, don't crash the run
        return {"error": f"keel native version failed: {e!r}"}
    if r.returncode != 0:
        return {"error": (r.stderr or r.stdout or "keel native version failed").strip()}
    try:
        return json.loads(r.stdout)
    except Exception as e:
        return {"error": f"unparseable keel native version output: {e!r}"}


def load_config():
    return json.loads(CONFIG_PATH.read_text())


def pin_env(cfg):
    """Export the pinned run config so harness modules pick it up at import time, then reconcile the
    *effective* values back into cfg so the manifest certifies what actually ran, not what the file
    said. A pre-set env var still wins (the `TRIALS=1 python3 run_suite.py` smoke-run affordance) —
    but because the effective trials flow into the manifest, a smoke run gets a *different* SHA that
    correctly won't match the pinned one, instead of silently certifying the pinned trial count."""
    os.environ.setdefault("TRIALS", str(cfg["run"]["trials"]))
    os.environ.setdefault("WORKERS", str(cfg["run"]["workers"]))
    cfg["run"]["trials"] = int(os.environ["TRIALS"])
    cfg["run"]["workers"] = int(os.environ["WORKERS"])


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
    print("  (stable SHA over each harness's scenarios + source (prompts, max_tokens),")
    print("   the shared bench_common source (judge, model constants), and the config)")
    return 0


def _keel_build_str():
    """A compact keel-build label for the markdown header, e.g. `0.1.0 @ cad424fd90f3 (dirty)`."""
    v = keel_version()
    if "error" in v:
        return f"unknown ({v['error']})"
    short = v.get("git_commit_short") or (v.get("git_commit") or "")[:12] or "no-commit"
    return f"{v.get('version', '?')} @ {short}" + (" (dirty)" if v.get("dirty") else "")


def run_compare(path_a, path_b, margin_pp=None):
    """Certify whether two report artifacts reproduce, via an equivalence test (TOST) at a margin, with
    a Holm-corrected regression check. Manifest-gated. Exit: 0 equivalent, 1 regression / manifest
    mismatch, 2 read/usage error, 3 inconclusive (underpowered — can't certify equivalence)."""
    try:
        ra = bench_compare.load_report(path_a)
        rb = bench_compare.load_report(path_b)
    except (OSError, ValueError) as e:
        print(f"could not read a report: {e}", file=sys.stderr)
        return 2
    margin = bench_compare.MARGIN_DEFAULT if margin_pp is None else margin_pp / 100.0
    r = bench_compare.compare(ra, rb, margin=margin)

    if r["verdict"] == "manifest_mismatch":
        print("✗ MANIFEST MISMATCH — the two runs used different inputs, so a reproduction check is meaningless.")
        print(f"    A: {r['manifest_a']}")
        print(f"    B: {r['manifest_b']}")
        return 1
    print(f"manifest match: {r['manifest_a']}")
    print(f"equivalence margin: ±{r['margin_pct']}pp   confidence: {round(100*(1-r['alpha']))}% (z={r['z']})")
    if not r["keel_match"]:
        print(f"note: different keel build (A {(r['keel_a'] or '?')[:12]} vs B {(r['keel_b'] or '?')[:12]}) — "
              "expected to still reproduce, but the tool changed.")
    if r["unmatched"]:
        print(f"note: benchmarks in only one report (skipped): {', '.join(r['unmatched'])}")
    print()

    for c in r["conditions"]:
        mark = "✗" if c["regressed"] else ("✓" if c["equivalent"] else "~")
        tag = "regressed" if c["regressed"] else ("equivalent" if c["equivalent"] else "inconclusive")
        print(f"  {mark} {c['id']:<16} {c['condition']:<8} "
              f"A {c['a'][0]}/{c['a'][1]} ({c['a_pct']}%)  B {c['b'][0]}/{c['b'][1]} ({c['b_pct']}%)  "
              f"Δ{c['diff_pct']:+}pp ±{c['ci_half_pct']}  p={c['p_val']}  [{tag}]")
    print()

    v = r["verdict"]
    if v == "no_data":
        print("reproduce check FAIL — no comparable conditions (empty or non-overlapping benchmark sets).")
        return 1
    if v == "regression":
        bad = [f"{c['id']}/{c['condition']}" for c in r["conditions"] if c["regressed"]]
        print(f"REGRESSION — {len(bad)} condition(s) differ significantly (Holm-corrected): {', '.join(bad)}.")
        return 1
    if v == "equivalent":
        n = len(r["conditions"])
        print(f"EQUIVALENT — all {n} condition(s) reproduce within ±{r['margin_pct']}pp. The run reproduced.")
        return 0
    # inconclusive: no significant regression, but the samples can't certify equivalence within margin
    widest = max(c["ci_half_pct"] for c in r["conditions"])
    weak = [f"{c['id']}/{c['condition']}" for c in r["conditions"] if not c["equivalent"]]
    print(f"INCONCLUSIVE — no significant regression, but {len(weak)} condition(s) can't be certified within "
          f"±{r['margin_pct']}pp at this sample size (widest CI half-width ±{widest}pp).")
    print(f"    underpowered: {', '.join(weak)}. Raise --margin, or add trials to tighten the interval.")
    return 3


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
           f"- keel build: `{_keel_build_str()}`",
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
    # Build (and validate) the manifest BEFORE spending any credits, so a config/harness drift or a
    # config-vs-code model mismatch fails loudly here rather than after a full paid run.
    manifest = bench_manifest.build_manifest(cfg)
    # Importing any harness pulls in bench_common, which resolved BENCH_PROVIDER at import — surface
    # the effective transport so a run's output says how it was routed.
    import bench_common
    print(f"transport: {bench_common.PROVIDER}")
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
        # Transport used (recorded, not hashed into the manifest — the models are identical across
        # providers, so a run over either routes stays comparable to the pinned reference).
        "provider": bench_common.PROVIDER,
        # Pin the exact inputs this result was produced from: anyone can recompute the manifest and
        # confirm a later run used byte-identical inputs (reproducibility).
        "manifest": manifest,
        # Run-environment provenance: WHICH keel build produced this result. Recorded (not hashed into
        # the manifest) because the keel commit advances with keel development — reproducing a result
        # means matching the manifest SHA AND this keel commit on a clean tree.
        "environment": {"keel": keel_version()},
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
    ap.add_argument("--verify-corpus", action="store_true",
                    help="check each real-corpus checkout on disk is at the pinned commit; NO API calls")
    ap.add_argument("--verify-keel", action="store_true",
                    help="print the keel binary's build identity and require a clean, identified commit; NO API calls")
    ap.add_argument("--compare", nargs=2, metavar=("REPORT_A", "REPORT_B"),
                    help="certify whether two report.json runs reproduce (equivalence test, manifest-gated); NO API calls")
    ap.add_argument("--margin", type=float, metavar="PP", default=None,
                    help="equivalence margin in percentage points for --compare (default 20)")
    ap.add_argument("--only", metavar="ID", help="run a single benchmark by its config id")
    ap.add_argument("--provider", choices=("anthropic", "openrouter"), default=None,
                    help="LLM transport: anthropic (default, ~/.claude-token) or openrouter "
                         "(~/.openrouter). Same pinned models either way; only the transport differs.")
    args = ap.parse_args()

    # Resolve the transport BEFORE any harness (and thus bench_common) is imported, since bench_common
    # reads BENCH_PROVIDER at import. Precedence: --provider > BENCH_PROVIDER env > anthropic.
    provider = args.provider or os.environ.get("BENCH_PROVIDER") or "anthropic"
    os.environ["BENCH_PROVIDER"] = provider

    if args.compare:
        return run_compare(*args.compare, margin_pp=args.margin)

    cfg = load_config()
    if args.manifest:
        print(json.dumps(bench_manifest.build_manifest(cfg), indent=2))
        return 0
    if args.verify_corpus:
        rows = bench_manifest.verify_corpus(cfg)
        if not rows:
            print("no corpus-pinned benchmarks in the config.")
            return 0
        bad = 0
        for r in rows:
            ok = r["status"] == "match"
            bad += 0 if ok else 1
            mark = "✓" if ok else "✗"
            note = f"  [{r['status']}]"
            if r["status"] == "dirty":
                note = f"  [dirty: {r['dirty']} tracked file(s) modified vs the pinned commit]"
            print(f"  {mark} {r['id']:<14} {r['repo']}")
            print(f"      expected {r['expected']}")
            print(f"      on disk  {r['actual'] or '(none)'}{note}  {r['path']}")
        print()
        if bad:
            print(f"corpus verify FAIL — {bad} checkout(s) not at the pinned revision (wrong commit or dirty tree).")
            return 1
        print(f"corpus verify PASS — {len(rows)} checkout(s) at the pinned commit with a clean tree.")
        return 0
    if args.verify_keel:
        v = keel_version()
        if "error" in v:
            print(f"  ✗ keel native version — {v['error']}")
            print("\nkeel verify FAIL — could not read the keel build identity (is the binary built and current?).")
            return 1
        commit = v.get("git_commit") or ""
        short = v.get("git_commit_short") or commit[:12]
        dirty = bool(v.get("dirty", False))
        print(f"  keel version {v.get('version', '?')}  commit {short or '(none)'}  dirty={str(dirty).lower()}")
        # reproducible iff the build is identified (has a commit) AND the source tree was clean
        if not commit:
            print("\nkeel verify FAIL — no embedded commit (keel built outside a checkout); build is not reproducible.")
            return 1
        if dirty:
            print("\nkeel verify FAIL — the keel build tree was dirty; rebuild from a clean commit to reproduce.")
            return 1
        print("\nkeel verify PASS — keel built from a clean, identified commit.")
        return 0
    if args.dry_run:
        return dry_run(cfg)
    return full_run(cfg, only=args.only)


if __name__ == "__main__":
    raise SystemExit(main())

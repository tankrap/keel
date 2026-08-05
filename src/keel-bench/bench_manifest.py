#!/usr/bin/env python3
"""Reproducibility manifest for the keel benchmark suite (NEW-1094).

A benchmark run is only a *proof* if its inputs are pinned and verifiable. This computes a stable
SHA-256 over the suite's pinned inputs — every benchmark's scenario table plus the result-affecting
config (models, trial/worker counts, Wilson z, scenario shape) — so a result can carry the exact
`manifest_sha256` it was produced under and anyone can recompute it to confirm the inputs are
byte-identical to a prior run.

No API calls, no keel binary, no token: it only imports the harness scenario tables (which is why
`run_suite.py --dry-run` can already import every harness for free). Deterministic by construction:
identical inputs → identical bytes → identical SHA.
"""
import hashlib
import importlib
import json


def _canon(obj) -> bytes:
    """Canonical JSON: sorted keys, compact separators, ASCII-escaped — so identical data always
    serializes to identical bytes regardless of dict insertion order or platform."""
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()


def _sha(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()


def scenario_sha(module_name: str):
    """Import a harness and hash its pinned scenario table (`SCEN`). Returns `(sha256, count)`.
    The scenario table *is* the pinned input — the conventions/tasks/lessons under test."""
    mod = importlib.import_module(module_name)
    scen = getattr(mod, "SCEN", None)
    if not isinstance(scen, (list, tuple)):
        raise ValueError(f"{module_name}.SCEN is missing or not a list")
    rows = [list(r) for r in scen]  # tuples → lists for stable JSON; content is unchanged
    return _sha(_canon(rows)), len(rows)


def config_sha(cfg: dict) -> str:
    """Hash only the config fields that affect results — models, run params, and each benchmark's
    shape. Descriptions, notes, and the key source are excluded (they don't change an outcome)."""
    core = {
        "schema": cfg["schema"],
        "version": cfg["version"],
        "models": {k: cfg["models"][k] for k in ("solver", "judge", "api_version")},
        "run": cfg["run"],
        "benchmarks": [
            {"id": b["id"], "module": b["module"], "scenarios": b["scenarios"], "scenario_arity": b["scenario_arity"]}
            for b in cfg["benchmarks"]
        ],
    }
    return _sha(_canon(core))


def build_manifest(cfg: dict) -> dict:
    """The full reproducibility manifest: per-benchmark scenario SHAs + the config SHA, bound into
    one `manifest_sha256`. Two runs share a manifest SHA iff their pinned inputs are byte-identical."""
    benches = []
    for b in cfg["benchmarks"]:
        sha, n = scenario_sha(b["module"])
        # cross-check the pinned scenario count so a config/table drift is caught here, not at run time
        pinned = b.get("scenarios")
        if pinned is not None and pinned != n:
            raise ValueError(f"{b['id']}: config pins {pinned} scenarios but {b['module']}.SCEN has {n}")
        benches.append({"id": b["id"], "module": b["module"], "scenarios": n, "scenario_sha256": sha})
    benches.sort(key=lambda x: x["id"])  # order-independent
    csha = config_sha(cfg)
    core = {"config_sha256": csha, "benchmarks": benches}
    return {
        "schema": cfg["schema"],
        "suite_version": cfg["version"],
        "config_sha256": csha,
        "benchmarks": benches,
        "manifest_sha256": _sha(_canon(core)),
    }


if __name__ == "__main__":
    # standalone: print the manifest for the pinned config (no API, no token)
    import pathlib

    cfg = json.loads((pathlib.Path(__file__).resolve().parent / "bench-config.json").read_text())
    print(json.dumps(build_manifest(cfg), indent=2))

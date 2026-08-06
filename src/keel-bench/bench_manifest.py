#!/usr/bin/env python3
"""Reproducibility manifest for the keel benchmark suite (NEW-1094).

A benchmark run is only a *proof* if its inputs are pinned and verifiable. This computes a stable
SHA-256 over the inputs that actually determine a result, so a report can carry the exact
`manifest_sha256` it was produced under and anyone can recompute it to confirm a later run used
byte-identical inputs.

What is bound, and why each is load-bearing:
  - each harness's **scenario table** (`SCEN`) — the conventions/tasks/lessons under test;
  - each harness's **full source** — because the solver system prompt, the per-scenario prompt
    assembly, and `max_tokens` live in the harness *code*, not in `SCEN`; two runs with different
    prompts must not share a SHA;
  - **`bench_common.py`'s source** — the shared dual-judge prompt, the `api()` defaults, and the
    `SOLVER`/`JUDGE`/`API_VERSION` constants the harnesses *actually call* (the run reads these, not
    the config's `models` block);
  - the **result-affecting config** — schema/version, the pinned model IDs (verified below to equal
    the constants the code uses, so the config can't silently drift from the run), `trials`, the
    Wilson `z`, and each benchmark's shape. `workers` is pure parallelism and is deliberately
    excluded — it changes wall-clock, never an outcome, so binding it would flag two identical-input
    runs as different.

Hashing source is conservative on purpose: a comment or whitespace edit bumps the SHA (a harmless
false "not reproducible"), which is the safe direction — far better than a prompt change slipping
through as a false "reproducible".

Not yet bound: the *content* of the real corpus checkouts (the copied file bytes). Pinning each
corpus's git commit is the tracked follow-up. The gap is small — the solver prompt is built from the
scenario's path/hint/task/lesson (all in `SCEN`), not the file bytes, so a different checkout moves
essentially only the reported retrieval-hit count, not the headline lift.

No API calls, no keel binary, no token: it only imports the harness modules (which is why
`run_suite.py --dry-run` can already import every harness for free). Deterministic by construction:
identical inputs -> identical bytes -> identical SHA.
"""
import hashlib
import importlib
import json
import pathlib

# The model constants the harnesses actually call live here; the manifest binds this file's source
# and checks the config's model IDs against these values.
COMMON_MODULE = "bench_common"


def _canon(obj) -> bytes:
    """Canonical JSON: sorted keys, compact separators, ASCII-escaped — so identical data always
    serializes to identical bytes regardless of dict insertion order or platform."""
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()


def _sha(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()


def scenario_sha(module_name: str):
    """Import a harness and hash its pinned scenario table (`SCEN`). Returns `(sha256, count)`.
    A semantic, comment-stable identity of the test set (`_source_sha` binds the rest of the file)."""
    mod = importlib.import_module(module_name)
    scen = getattr(mod, "SCEN", None)
    if not isinstance(scen, (list, tuple)):
        raise ValueError(f"{module_name}.SCEN is missing or not a list")
    rows = [list(r) for r in scen]  # tuples → lists for stable JSON; content is unchanged
    return _sha(_canon(rows)), len(rows)


def _source_sha(module_name: str) -> str:
    """Hash a module's source bytes — binds the prompts, prompt-assembly, and `max_tokens` that live
    in code rather than in `SCEN`. Conservative: any edit to the file changes this."""
    mod = importlib.import_module(module_name)
    return _sha(pathlib.Path(mod.__file__).read_bytes())


def _verify_models(cfg: dict):
    """The harnesses call `bench_common.SOLVER/JUDGE/API_VERSION`, not the config's `models` block.
    Refuse to certify a config whose pinned model IDs disagree with the constants the run uses, so the
    `models` field can never become decorative. Raises ValueError on drift."""
    bc = importlib.import_module(COMMON_MODULE)
    actual = {"solver": bc.SOLVER, "judge": bc.JUDGE, "api_version": bc.API_VERSION}
    pinned = {k: cfg["models"][k] for k in actual}
    if actual != pinned:
        raise ValueError(
            f"config models {pinned} != the constants the run uses {actual} "
            f"(update bench-config.json or {COMMON_MODULE}.py so they agree)"
        )


def config_sha(cfg: dict) -> str:
    """Hash only the config fields that affect a result — schema/version, the (code-verified) model
    IDs, `trials`, the Wilson `z`, and each benchmark's shape. `workers` (parallelism), descriptions,
    notes, and the key source are excluded: they don't change an outcome. Benchmarks are sorted so
    reordering the config array can't change the SHA."""
    _verify_models(cfg)

    def _bench_core(b):
        c = {"id": b["id"], "module": b["module"], "scenarios": b["scenarios"],
             "scenario_arity": b["scenario_arity"]}
        # a real-corpus benchmark's inputs include WHICH revision of the real files it ran against;
        # pin the repo + commit so a different checkout yields a different SHA (the src path/env don't
        # affect a result — only the repo identity and commit do).
        corpus = b.get("corpus")
        if corpus:
            c["corpus"] = {"repo": corpus["repo"], "commit": corpus["commit"]}
        return c

    core = {
        "schema": cfg["schema"],
        "version": cfg["version"],
        "models": {k: cfg["models"][k] for k in ("solver", "judge", "api_version")},
        "run": {k: cfg["run"][k] for k in ("trials", "wilson_z")},  # workers excluded on purpose
        "benchmarks": sorted((_bench_core(b) for b in cfg["benchmarks"]), key=lambda x: x["id"]),
    }
    return _sha(_canon(core))


def build_manifest(cfg: dict) -> dict:
    """The full reproducibility manifest: per-benchmark scenario + source SHAs, the shared
    `bench_common` source SHA, and the config SHA, bound into one `manifest_sha256`. Two runs share a
    manifest SHA iff every result-determining input is byte-identical."""
    csha = config_sha(cfg)  # also verifies the pinned models equal the constants the run uses
    benches = []
    for b in cfg["benchmarks"]:
        sha, n = scenario_sha(b["module"])
        # cross-check the pinned scenario count so a config/table drift is caught here, not at run time
        pinned = b.get("scenarios")
        if pinned is not None and pinned != n:
            raise ValueError(f"{b['id']}: config pins {pinned} scenarios but {b['module']}.SCEN has {n}")
        entry = {
            "id": b["id"],
            "module": b["module"],
            "scenarios": n,
            "scenario_sha256": sha,
            "source_sha256": _source_sha(b["module"]),  # binds this harness's prompts + max_tokens
        }
        if b.get("corpus"):  # record the pinned real-files revision this benchmark ran against
            entry["corpus"] = {"repo": b["corpus"]["repo"], "commit": b["corpus"]["commit"]}
        benches.append(entry)
    benches.sort(key=lambda x: x["id"])  # order-independent
    common = _source_sha(COMMON_MODULE)  # binds the shared judge prompt, api() defaults, model constants
    core = {"config_sha256": csha, "common_source_sha256": common, "benchmarks": benches}
    return {
        "schema": cfg["schema"],
        "suite_version": cfg["version"],
        "config_sha256": csha,
        "common_source_sha256": common,
        "benchmarks": benches,
        "manifest_sha256": _sha(_canon(core)),
    }


def verify_corpus(cfg: dict):
    """Check that each real-corpus checkout on disk actually holds the pinned revision's files —
    i.e. HEAD equals the pinned commit AND the tracked tree is clean. A matching HEAD alone is not
    enough: an uncommitted/staged edit to a corpus file changes the bytes the benchmark reads while
    HEAD is unchanged, so this also runs `git status --porcelain` (tracked files, submodules) and
    fails a checkout that is `dirty`.

    Separate from `build_manifest` on purpose: it shells out to git and needs the checkout present,
    so it is opt-in (CI and `--manifest` stay checkout-free). Returns one row per corpus benchmark
    with a `status`: `match` | `MISMATCH` (HEAD) | `dirty` (HEAD matches but tree modified) | `absent`
    (no dir) | `not-a-git-checkout`. Never raises for a missing corpus — the caller decides whether an
    absent/mismatched/dirty corpus is fatal. Untracked files are not flagged (they don't change the
    pinned tracked files the benchmark reads); LFS/symlink content is a documented residual."""
    import os
    import subprocess

    def _git(src, *args):
        return subprocess.run(["git", "-C", src, *args], capture_output=True, text=True, timeout=30)

    rows = []
    for b in cfg["benchmarks"]:
        c = b.get("corpus")
        if not c:
            continue
        src = os.environ.get(c.get("src_env", ""), "") or os.path.expanduser(c["src_default"])
        row = {"id": b["id"], "repo": c["repo"], "expected": c["commit"], "path": src, "dirty": None}
        git_dir = pathlib.Path(src, ".git")
        if not pathlib.Path(src).is_dir() or not git_dir.exists():
            row["status"] = "absent" if not pathlib.Path(src).is_dir() else "not-a-git-checkout"
            row["actual"] = None
        else:
            try:
                actual = _git(src, "rev-parse", "HEAD").stdout.strip()
            except Exception as e:  # git missing / not a repo — report, don't crash
                actual = f"<error: {e!r}>"
            row["actual"] = actual
            if actual != c["commit"]:
                row["status"] = "MISMATCH"
            else:
                # HEAD matches — now require the tracked tree to be clean, or the bytes may differ.
                try:
                    porcelain = _git(src, "status", "--porcelain", "--untracked-files=no",
                                     "--ignore-submodules=none").stdout.strip()
                except Exception as e:
                    porcelain = f"<error: {e!r}>"
                n_dirty = len([ln for ln in porcelain.splitlines() if ln]) if porcelain else 0
                row["dirty"] = n_dirty
                row["status"] = "dirty" if porcelain else "match"
        rows.append(row)
    return rows


if __name__ == "__main__":
    # standalone: print the manifest for the pinned config (no API, no token)
    cfg = json.loads((pathlib.Path(__file__).resolve().parent / "bench-config.json").read_text())
    print(json.dumps(build_manifest(cfg), indent=2))

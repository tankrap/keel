# keel benchmark suite

Two families of benchmarks live here.

1. **The flywheel benchmarks** (live LLM, real API calls). The core question: does keel's retrieval actually make an agent more correct? Most of this README is about these.
2. **The deterministic gate and scale benchmarks** (`ci_gate.sh`, `run_all.sh`, `scale_bench.rs`, `system_bench.rs`, `c_scale.rs`). Free, no API key, run in CI. See [Deterministic gate and scale](#deterministic-gate-and-scale) at the bottom.

The results of record are in [SUITE-RESULTS.md](./SUITE-RESULTS.md). This README is how to run them.

---

## The flywheel benchmarks

What they test: an agent is more correct when keel surfaces the non-obvious prior lesson for a task. Each scenario carries a project convention a capable model cannot guess on its own. keel records the convention in a real repo (`keel learn`) and retrieves it (`keel brief`). We solve each task without the brief and with it, several trials each, and a strict dual judge (both votes must agree) scores whether the result follows the rule. The lift is the with-rate minus the without-rate.

### Results

Solver `claude-opus-4-8`, judge `claude-sonnet-5`, 4 trials per scenario per condition, Wilson 95% intervals.

| benchmark | harness | without | with | lift |
|---|---|---|---|---|
| synthetic, invented rules | `flywheel_bench.py` | 0% | 95% | +95 |
| vs code, typescript | `corpus_bench.py` | 73% | 94% | +20 |
| django, python | `corpus_bench_django.py` | 62% | 92% | +29 |
| prometheus, go | `corpus_bench_go.py` | 58% | 90% | +31 |
| tokio, rust | `corpus_bench_rust.py` | 60% | 98% | +38 |
| pooled real corpus, 4 languages | | 64% | 93% | +29 |

The synthetic run isolates the mechanism at maximum headroom, where the model can't guess and the baseline is near zero. The four real corpora show it still pays against a model that already knows the language, on the subset of conventions it hadn't memorized, which is the only subset retrieval can help. Every convention was retrieved: 72 of 72 across the real corpora, 20 of 20 synthetic. The lift concentrates on repo-local idioms the model rarely internalizes, so the largest is on tokio (Rust, +38), the language keel itself is written in. The full write-up, including the conventions that don't move even with the lesson (model ceilings, not retrieval misses), is in [SUITE-RESULTS.md](./SUITE-RESULTS.md).

### Adversarial control

`flywheel_adversarial.py` adds a third condition on the synthetic scenarios: a decoy, a different scenario's real rule, plausible and authoritative-looking but wrong for the task. A wrong retrieved rule gives 0% lift, identical to no lesson at all, so the gain is the specific retrieved rule and not the effect of adding more text to the prompt. It runs standalone because it has three conditions rather than the without/with pair the rest of the suite aggregates.

### Cost

A live run calls the Anthropic API thousands of times (each scenario, times conditions, times trials, and each solve graded by two judge calls). It costs real API credits. Only `--dry-run` is free. Don't run the live suite casually or in CI.

### How to run

```sh
# Free: import every harness and check the scenario tables parse. No API calls, no key.
python3 run_suite.py --dry-run

# Free: print the reproducibility manifest (a stable SHA over every pinned input). No API, no key.
python3 run_suite.py --manifest

# Free: check each real-corpus checkout on disk is at the manifest's pinned commit. No API, no key.
python3 run_suite.py --verify-corpus

# Free: print the keel binary's build identity and require a clean, identified commit. No API, no key.
python3 run_suite.py --verify-keel

# Free: do two report.json runs reproduce? (equivalence test, manifest-gated). No API, no key.
python3 run_suite.py --compare run_a.json run_b.json          # default ±20pp margin
python3 run_suite.py --compare run_a.json run_b.json --margin 15

# Live (costs credits): run the packaged benchmarks and write an aggregated report.
python3 run_suite.py

# Live, one benchmark by id:
python3 run_suite.py --only corpus-django

# A single harness directly (same pinned defaults):
python3 flywheel_bench.py          # synthetic
python3 corpus_bench.py            # vs code
python3 corpus_bench_django.py     # django
python3 corpus_bench_go.py         # prometheus / go
python3 corpus_bench_rust.py       # tokio / rust
python3 flywheel_adversarial.py    # adversarial control (standalone)
```

`run_suite.py` writes two aggregated artifacts on a live run: `bench-report.json` (every metric with its Wilson 95% interval) and `bench-report.md` (the human form, with the reference numbers alongside).

Prerequisites for a live run:

- An Anthropic key at `~/.claude-token`, read lazily and never printed or logged.
- The keel release binary at `~/keel/src/rust/target/release/keel` (override with `KEEL_BIN`), built with `cargo build --release` in `src/rust`.
- For the real corpora, a checkout of each repo. vs code defaults to `~/keel-vscode-demo`, django to `~/keel-django-demo`, prometheus to `~/keel-go-demo`, tokio to `~/keel-rust-demo`. Point at your own with `CORPUS_SRC`.

### Pinned config

Solver, judge, trials, workers, and the Wilson z are pinned in [bench-config.json](./bench-config.json) so runs stay comparable over time. `run_suite.py` reads it and exports `TRIALS`/`WORKERS` before importing the harnesses, and an env var you set yourself still wins for a quick smoke run. No temperature is ever sent: opus-4-8's adaptive thinking forces temp=1, and sending a temperature returns HTTP 400. Bump `version` in that file whenever the models, scenario tables, or trial design change, so older reports stay comparable like-for-like.

### Reproducibility manifest

A benchmark is only a *proof* if the inputs that determine a result are pinned and verifiable. `bench_manifest.py` computes a stable SHA-256 over exactly those inputs, and every live report carries the `manifest_sha256` it was produced under. Anyone can `python3 run_suite.py --manifest` and confirm a later run used byte-identical inputs.

What it binds, and why each matters:

- **each harness's scenario table** (`SCEN`) — the conventions/tasks/lessons under test;
- **each harness's source** — because the solver prompt, the per-scenario prompt assembly, and `max_tokens` live in the harness *code*, not in `SCEN`; two runs with different prompts must not share a SHA;
- **`bench_common.py`'s source** — the shared dual-judge prompt, the `api()` defaults, and the `SOLVER`/`JUDGE`/`API_VERSION` constants the harnesses *actually call* (the run reads these, not the config's `models` block);
- **the result-affecting config** — schema/version, the pinned model IDs (verified to equal the constants the code uses, so the config can't silently drift from the run), `trials`, the Wilson `z`, and each benchmark's shape.

- **each real corpus's `repo` + git `commit`** — the exact revision of the real files a corpus benchmark ran against, so a different checkout yields a different SHA. (The `src` path/env are *not* hashed — only the repo identity and commit affect a result.)

`workers` is deliberately **excluded** (pure parallelism — it changes wall-clock, never an outcome). Hashing source is conservative on purpose: a comment edit bumps the SHA (a harmless false "not reproducible"), the safe direction.

`python3 run_suite.py --verify-corpus` checks that each corpus checkout **on disk** actually holds the pinned revision's files: HEAD equals the pinned commit **and** the tracked tree is clean (`git status --porcelain`). A matching HEAD alone is not enough — a staged/uncommitted edit to a corpus file changes the bytes the benchmark reads while HEAD is unchanged, so a modified tree is reported `dirty` and fails. Untracked files aren't flagged (they don't change the pinned tracked files the benchmark reads); LFS-pointer/symlink content is a documented residual. It's kept out of `build_manifest` and the CI gate because it needs the (large) checkouts present; the manifest itself stays checkout-free. Because all corpora share the `CORPUS_SRC` env, run it with `CORPUS_SRC` **unset** so each resolves to its own default checkout.

**The keel binary is recorded, not hashed.** The keel build determines `brief`/`learn` retrieval, so it's a genuine input — but its commit advances with keel's own development, so pinning it into the manifest SHA would make the SHA churn on every keel commit and be un-pinnable ahead of a build. Instead, a live report records the keel build identity (`keel native version` → crate version + the git commit it was built from + whether that tree was dirty) under `environment.keel`, and `python3 run_suite.py --verify-keel` requires the on-disk binary to report a clean, identified commit. So the full reproducibility contract is: **match the manifest SHA (static inputs) *and* the report's `environment.keel` commit on a clean tree (the tool).** (`keel native version` is keel's own build stamp; plain `keel --version` proxies to the git binary keel wraps.) The stamp is exact for a full build from a clean checkout, which is how you should build the binary you benchmark (`cargo build --release` in a clean `src/rust`); on incremental dev rebuilds the `dirty` flag is best-effort. Nothing auto-runs `--verify-keel` before a paid run — a dirty keel is *recorded* (`environment.keel.dirty: true`), not refused, so check it before trusting a result.

**Reproducing the stochastic (LLM) parts.** The manifest makes the *inputs* verifiable, but the LLM calls are stochastic — the same inputs won't reproduce byte-for-byte, so "reproducible" for the measured rates can't mean equal counts. It also can't mean "a significance test found no difference": *absence of evidence isn't evidence of reproduction*, and at these sample sizes (48–80/condition) a plain two-proportion test is too underpowered to catch a real 15-point regression, so using it to declare "reproduced" would give false confidence.

So `python3 run_suite.py --compare run_a.json run_b.json` certifies via an **equivalence test (TOST)**: two runs reproduce *within a margin δ* only if the 95% CI for their per-condition difference lies entirely inside (−δ, +δ). Because equivalence is what you must reject the null to conclude, low power makes this **conservative** — too few samples yields *inconclusive*, never a false *reproduced*. It reports one of three verdicts (manifest-gated — differing `manifest_sha256` ⇒ different inputs ⇒ hard mismatch; a differing `environment.keel` commit is a note):

- **EQUIVALENT** (exit 0) — every condition's difference CI ⊂ ±δ. The honest "it reproduced."
- **REGRESSION** (exit 1) — some condition differs significantly, via a **Holm-corrected** two-proportion test (the correction stops ~40% of truly-reproducing runs from being failed by chance across the many conditions).
- **INCONCLUSIVE** (exit 3) — neither: the samples can't certify equivalence within δ. The output prints the achievable CI half-width so you know the resolution.

δ defaults to **±20 percentage points** — not a quality bar but the *floor the sample size allows*: at n=48 and a 50% base rate even two identical runs can only be certified within ≈±20pp at 95%. Tighten it with `--margin PP` once you raise the trial count (more trials ⇒ narrower CIs ⇒ smaller certifiable δ). So confirming a re-run of a published result reproduces means: same manifest SHA, and `--compare` returns **EQUIVALENT**.

It's deterministic by construction (canonical JSON → identical bytes → identical hash), needs no API or token, and `--dry-run` asserts that determinism. `test_manifest.py` locks the guarantees (deterministic, scenario/source/config/corpus-commit change → SHA-change, `workers` change → SHA-*un*changed, config-vs-code model drift refused, count-drift caught), and the CI gate runs the self-test + dry-run so a scenario-table drift or a broken harness fails the build. Bump `version` in `bench-config.json` whenever the pinned inputs change.

### Reading the Wilson intervals

Each condition reports a Wilson score interval, the range the true success rate plausibly sits in for a finite sample. It is the right interval near 0% or 100%, where the naive interval can run below 0 or above 1.

- Disjoint intervals mean a real effect. In the synthetic run without is 0–5% and with is 88–98%, so the jump is not a run-to-run artifact.
- Overlapping intervals mean treat the gap as noise and collect more trials before claiming a difference.
- Intervals shrink as the trial count grows.

### Files

| file | role |
|---|---|
| `run_suite.py` | single entry point; `--dry-run` / `--manifest` / `--verify-corpus` / `--verify-keel`, aggregates a JSON and markdown report |
| `bench-config.json` | pinned config: models, trials, workers, scenario counts, Wilson z, per-corpus repo+commit |
| `bench_manifest.py` | reproducibility manifest: a stable SHA over every pinned input (no API) |
| `test_manifest.py` | self-test locking the manifest's guarantees (run in the CI gate) |
| `bench_compare.py` | reproduce check: TOST equivalence + Holm-corrected regression over two reports, manifest-gated (no API) |
| `test_compare.py` | self-test locking the comparator's guarantees (run in the CI gate) |
| `bench_common.py` | shared plumbing: the API client, `wilson()`, dual `judge()`, `sh()`, parallel `run_trials()` |
| `flywheel_bench.py` | synthetic conventions, 20 scenarios |
| `corpus_bench.py` | vs code, 16 scenarios |
| `corpus_bench_django.py` | django, 12 scenarios |
| `corpus_bench_go.py` | prometheus / go, 12 scenarios |
| `corpus_bench_rust.py` | tokio / rust, 12 scenarios |
| `flywheel_adversarial.py` | the without / with / decoy control (standalone) |
| `SUITE-RESULTS.md` | the results of record, with the full write-up |
| `flywheel-result.md`, `corpus-result.md` | earlier single-run write-ups |
| `report-*.json`, `bench-report.*` | generated by a live run (git-ignored) |

---

## Deterministic gate and scale

Free, no API key. The deterministic bars that fail the build on any regression.

| harness | what it proves | headline (this machine) |
|---|---|---|
| `ci_gate.sh` | build, tests, clippy, relevance, liveness, feedback, git on-ramp, status | all gates green |
| `scale_bench.rs` | storage and VCS core at scale: ingest, store size, `status`, incremental, GC | linux 94.5k files: ingest 37.5s, store 71%, status 0.5s |
| `system_bench.rs <dir> <n>` | relevance: cross-file symbol-slice over an `n`-target sample | vs code n=100: 97% cross-file, median 310-token brief |
| `c_scale.rs` | live include-graph and blast radius on C at scale | linux: 337k edges, 25.7s |

One entry point: `./run_all.sh` (add `--scale`, `--llm`, or `--all`). Reproduce the scale numbers by cloning the pinned inputs first:

```sh
git clone --depth 1 --filter=blob:none https://github.com/torvalds/linux   /Users/justin/keel-scale/linux
git clone --depth 1 --filter=blob:none https://github.com/microsoft/vscode /Users/justin/keel-scale/vscode
```

Numbers are single-machine (Apple Silicon, release build), directional, and reproducible via the commands above.

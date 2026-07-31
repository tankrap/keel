# keel benchmark suite

Two families of benchmarks live here:

1. **The flywheel benchmark suite (LLM, live API)** — the moat. Does keel's retrieval actually
   make an agent *more correct*? Packaged, versioned, and reproducible (NEW-1094). This is the
   focus of the rest of this README.
2. **The deterministic gate + scale benches** — `ci_gate.sh`, `run_all.sh`, `scale_bench.rs`,
   `system_bench.rs`, `c_scale.rs`. Free, no API key, run in CI. See
   [Deterministic gate & scale](#deterministic-gate--scale) at the bottom.

---

## The flywheel benchmark suite

**Claim under test:** an agent is more correct when keel surfaces the non-obvious prior lesson for
a task. Each scenario carries a project convention a capable model can't guess a priori. The
convention is recorded in a real keel repo (`keel learn`) and retrieved by `keel brief`. We solve
each task **WITHOUT** the brief and **WITH** it, N trials each, and a strict dual LLM judge scores
rule-compliance. **Lift = WITH% − WITHOUT%.**

### What each benchmark measures

| benchmark (`id`) | harness | what it proves | reference result |
|---|---|---|---|
| `flywheel-synthetic` | `flywheel_bench.py` | Lift at **maximum headroom**: 20 arbitrary conventions the model has never seen, so the baseline is ~0 and any success is pure retrieval. | WITHOUT **0/60 (0%)** → WITH **57/60 (95%)**, **+95** |
| `corpus-vscode` | `corpus_bench.py` | Lift on a **real codebase the model already knows**: 16 documented VS Code conventions attached to their real files. Answers "you invented those conventions." | WITHOUT **36/48 (75%)** → WITH **45/48 (94%)**, **+19** |

The two bracket the effect: the synthetic run isolates the mechanism at maximum headroom; the
real-corpus run shows it still pays against a knowledgeable model — on exactly the subset of
knowledge that isn't already in the weights, the only subset retrieval can ever help. Full write-ups
are in [`flywheel-result.md`](./flywheel-result.md) and [`corpus-result.md`](./corpus-result.md).

### ⚠️ Cost warning

A live run calls the Anthropic API thousands of times (each scenario × 2 conditions × `TRIALS`
solves, each solve graded by 2 judge calls). **It costs real API credits.** Only `--dry-run` is
free. Never run the live suite casually or in CI.

### How to run

```sh
# Free: import every harness + validate the scenario tables parse. NO API calls, NO token needed.
python3 run_suite.py --dry-run

# LIVE (costs credits): run both harnesses under the pinned config, write an aggregated report.
python3 run_suite.py

# LIVE, one benchmark only:
python3 run_suite.py --only flywheel-synthetic

# A single harness directly (same pinned defaults):
python3 flywheel_bench.py
python3 corpus_bench.py
```

`run_suite.py` writes two aggregated artifacts on a live run: **`bench-report.json`** (machine
form, every metric with its Wilson 95% CI) and **`bench-report.md`** (human form, with the
reference bar for comparison).

**Prerequisites for a live run:**
- API key at `~/.claude-token` (read lazily, cached in-process, **never printed or logged**).
- keel release binary at `~/keel/src/rust/target/release/keel` (override with `KEEL_BIN`).
  Build it with `cargo build --release` in `src/rust`.
- For `corpus-vscode` only: a `microsoft/vscode` checkout, default `~/keel-vscode-demo`
  (override with `CORPUS_SRC`).

### Pinned config (reproducibility)

Everything that makes a run reproducible and comparable over time is pinned in
[`bench-config.json`](./bench-config.json) — schema/version, models, trials, workers, scenario
counts, and the Wilson z. `run_suite.py` reads it and exports `TRIALS`/`WORKERS` before importing
the harnesses (an env var you set yourself still wins, for quick smoke runs).

| field | value | why |
|---|---|---|
| solver | `claude-opus-4-8` | the capable agent under test |
| judge | `claude-sonnet-5` | strict compliance grader (dual vote, both must agree) |
| temperature | **none sent** | opus-4-8 adaptive thinking forces temp=1; sending a temperature returns HTTP 400 |
| TRIALS | 3 | per scenario per condition; absorbs run-to-run variance |
| WORKERS | 6 | ThreadPoolExecutor parallelism, with retry/backoff on 429/500/529 |
| Wilson z | 1.96 | 95% confidence interval |

Bump `version` in `bench-config.json` whenever the models, scenario tables, or trial design change,
so past `bench-report.json` files stay comparable to like-for-like.

### How to read the Wilson 95% CIs

Each condition reports a **Wilson score interval** — the range the true success rate plausibly sits
in given a finite sample (here 48–60 samples per condition). It's the right interval for
proportions near 0% or 100%, where the naive `p ± 1.96·√(p(1−p)/n)` interval breaks (it can even
run below 0 or above 1).

- **Disjoint CIs ⇒ a real effect.** In the synthetic run WITHOUT is 0–6% and WITH is 86–98% — the
  intervals don't overlap, so the 0→95 jump is not a run-to-run artifact.
- **Overlapping CIs ⇒ treat the gap as noise.** Two conditions whose intervals overlap are not
  distinguishable at this sample size; collect more trials before claiming a difference.
- **Wider interval ⇒ less certainty.** Intervals shrink as `TRIALS` (hence `n`) grows. The
  real-corpus run's narrower headroom (75→94) still separates because the lift lands on the
  specific conventions the model hadn't internalized.

The reference bar to reproduce: **synthetic 0→95** and **real-corpus 75→94**. A packaged run is
healthy if each benchmark lands within its reference CI.

### Files

| file | role |
|---|---|
| `run_suite.py` | single entry point; `--dry-run`, aggregates JSON + markdown report |
| `bench-config.json` | pinned, versioned config (models, trials, workers, scenario counts, Wilson z) |
| `bench_common.py` | shared plumbing: `api()` client, `wilson()`, dual `judge()`, `sh()`, parallel `run_trials()` |
| `flywheel_bench.py` | synthetic-convention harness (20 scenarios) — exposes `SCEN` + `run()` |
| `corpus_bench.py` | real VS Code harness (16 scenarios) — exposes `SCEN` + `run()` |
| `flywheel-result.md`, `corpus-result.md` | reference write-ups of the known results |
| `bench-report.json`, `bench-report.md` | generated by a live `run_suite.py` (git-ignored churn) |

---

## Deterministic gate & scale

Free, no API key — the deterministic bars that fail the build on any regression.

| harness | what it proves | headline (this machine) |
|---|---|---|
| `ci_gate.sh` | build · tests · clippy · relevance · liveness · feedback · git on-ramp · status | all gates green |
| `scale_bench.rs` | storage/VCS core at scale: ingest, store size, `status`, incremental, GC | linux 94.5k files: ingest 37.5s, store 71%, status 0.5s |
| `system_bench.rs <dir> <n>` | relevance: cross-file symbol-slice over an `n`-target sample | VS Code n=100: 97% cross-file, median 310-tok brief |
| `c_scale.rs` | live include-graph + blast radius on C at scale | linux: 337k edges, 25.7s |

One entry point: `./run_all.sh` (add `--scale`, `--llm`, or `--all`). Reproduce the scale numbers by
cloning the pinned inputs first:

```sh
git clone --depth 1 --filter=blob:none https://github.com/torvalds/linux   /Users/justin/keel-scale/linux
git clone --depth 1 --filter=blob:none https://github.com/microsoft/vscode /Users/justin/keel-scale/vscode
```

Numbers are single-machine (Apple Silicon, release build) — directional, reproducible via the
commands above.

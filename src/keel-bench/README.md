# keel benchmark suite

The proof + the gate. One entry point: `./run_all.sh` (add `--scale`, `--llm`, or `--all`).

## What each piece measures

| harness | what it proves | headline (this machine) |
|---|---|---|
| `ci_gate.sh` | build · tests · clippy · relevance · liveness · feedback · git on-ramp · status — **fails the build on any regression** | all gates green |
| `scale_bench.rs` | storage/VCS core at scale: ingest, store size, `status`, incremental, GC | linux 94.5k files: ingest 37.5s, store 71%, status 0.5s |
| `system_bench.rs <dir> <n>` | relevance: cross-file symbol-slice over an `n`-target sample | VS Code n=100: 97% cross-file, median 310-tok brief |
| `c_scale.rs` | live include-graph + blast radius on C at scale | linux: 337k edges, 25.7s |
| `flywheel_live.py` | **the moat**: does keel's retrieval lift agent correctness? (LLM, dual-judge) | WITHOUT 70% → WITH 100%, +30pts |

## keel vs git / Entire / Sourcegraph
See the published report (marine-themed HTML). Honest summary: git wins storage (~4× smaller, delta+pack) and ties `status`; keel wins ingest and — the point — the context git/Entire/Sourcegraph have **no equivalent** for (live cross-file relevance, blast radius, the fused brief, the flywheel).

## Reproducing the scale numbers
Clone the pinned inputs, then `./run_all.sh --all`:
```
git clone --depth 1 --filter=blob:none https://github.com/torvalds/linux   /Users/justin/keel-scale/linux
git clone --depth 1 --filter=blob:none https://github.com/microsoft/vscode /Users/justin/keel-scale/vscode
```

## Honesty notes (what's NOT yet in the suite)
- **Wilson confidence intervals** on the LLM-graded runs and a pinned manifest SHA for byte-stable determinism — not yet wired (NEW-1094 remaining).
- The flywheel eval uses **authored** lessons (no public corpus of real agent sessions); code, retrieval, agent, and dual-judge grading are real. Real-corpus + weaker-agent validation is the follow-up (NEW-1105).
- Numbers are single-machine (Apple Silicon, release build) — directional, reproducible via the commands above.

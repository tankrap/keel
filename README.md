# keel

keel is version control for agent-written code. It is git-compatible. A plain `git push` to a keel repo lands as native history. Clone, fetch, pull, and push work unchanged.

keel stores history as voyages. A voyage is one agent session. Every read returns the cargo for a task in one fetch: the task code, the dependency chart, provenance, and prior voyages. git returns diffs and leaves context assembly to the reader.

## Benchmarks that matter

Measured, with 95% confidence intervals. Reproduce from `src/keel-bench`.

### Agent correctness (the flywheel)

keel records a project convention on one voyage. It surfaces that convention as cargo on the next. Measured lift in rule compliance. Solver claude-opus-4-8, judge claude-sonnet-5, 4 trials per case.

| corpus | without cargo | with cargo | lift |
|---|---|---|---|
| synthetic, invented rules | 0% | 95% | +95 |
| vs code, typescript | 73% | 94% | +20 |
| django, python | 62% | 92% | +29 |
| prometheus, go | 58% | 90% | +31 |
| pooled real, 3 languages | 66% | 92% | +26 |

Adversarial control: a wrong convention retrieved as cargo gives +0 lift. Identical to no cargo. The lift is the specific rule, not extra context.

The pattern holds across three languages. As a language's conventions are less present in the model, the baseline falls and the lift rises.

### Read speed

`keel status` on the linux kernel, 80,000 files, median of 5.

| | time |
|---|---|
| keel status via the daemon | under 10ms |
| keel status, cold walk | 0.42s |
| git status | 0.26s |

The daemon holds a warm index and answers in O(changed). git wins cold. keel wins once the daemon is up.

## keel vs git

What keel adds.

- cargo in one fetch. Context, chart, provenance, prior voyages. git returns diffs.
- the flywheel. Conventions learned on one voyage lift correctness on the next. Numbers above. git has no session memory.
- provenance. Every change signs to a human. An agent voyage always chains to a person. git records an author string, unsigned by default.
- content-addressed storage. BLAKE3 objects, chunked with FastCDC, delta compressed. Any object verifies by its hash.

Where git wins.

- shallow clones. git is faster.
- pack size. keel repack cuts pack size 36% on a 300-commit import. git still packs smaller.
- tooling. git has 20 years of it and near-universal support. keel is new.
- human-only work. git is smaller and simpler when no agent reads history.

## Benchmark your own repo

The flywheel test is: take conventions your repo enforces, record each with `keel learn`, then measure whether `keel brief` retrieval makes an agent comply.

```
# build keel
cd src/rust && cargo build --release

# validate the harnesses. no api calls. free.
cd ../keel-bench && python3 run_suite.py --dry-run

# run a published benchmark. needs an anthropic key in ~/.claude-token.
TRIALS=4 python3 flywheel_bench.py

# your repo: copy a corpus harness, point it at your checkout,
# replace the convention list with rules your repo enforces.
cp corpus_bench_django.py corpus_bench_mine.py
# edit SCEN to (file, hint, task, rule, check), then:
CORPUS_SRC=/path/to/your/repo python3 corpus_bench_mine.py
```

Each scenario is one convention: the file it governs, the task, the rule, and the check the judge applies. keel learns the rule, brief retrieves it, the agent solves the task with and without it, the judge scores compliance. See `src/keel-bench/SUITE-RESULTS.md`.

## Repo layout

Rust workspace under `src/rust`.

- keel-store · object store. content-addressed, BLAKE3, FastCDC chunks, delta compression, LMDB.
- keel-resolve · language resolvers. import and symbol graphs, per language, as sidecars.
- keel-graph · the live chart. dependency graph, kept warm.
- keel-brief · fuses the cargo. task code, chart, provenance, prior voyages, one fetch.
- keel-coord · coordination. reservations and conflict prediction across voyages.
- keel-git · git compatibility. byte-identical codec, packfiles, smart-http server, mirror both ways.
- keel-net · transport. QUIC. object fetch by hash, live events.
- keel-daemon · keeld. holds the store, chart, and warm status. answers reads fast.
- keel-cmd · the `keel` binary. a drop-in for git.
- keel-core · shared types.

Benchmarks are in `src/keel-bench`. Design docs are in `src/docs`.

## Build

```
cd src/rust
cargo build --release    # target/release/keel, target/release/keeld
cargo test --release     # 20 suites
```

## Status

keel is early. Verified today.

- git compatibility. clone, fetch, pull, push. a git push lands as native history. codec byte-identical over 47,000 real objects.
- status and diff match git, including gitignore and symlinks.
- the flywheel, benchmarked above.
- the daemon. warm cargo, O(changed) status, coordination over QUIC.

Not yet. pack size at git parity. hosted multi-repo serving, which is hull. a frozen on-disk format.

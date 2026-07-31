# keel

keel is version control for AI-written code. It is git-compatible. A plain `git push` to a keel repo lands as a normal keel commit. Clone, fetch, pull, and push all work unchanged.

The difference from git: keel treats each agent session as a first-class unit of history, and it answers a read with one fetch that returns everything an agent needs for a task. The relevant code, the dependency graph, a signed record of who wrote what, and lessons from past sessions. git returns diffs and leaves the rest to you.

## Benchmarks that matter

Measured, with 95% confidence intervals. Reproduce from `src/keel-bench`.

### Does it make agents more correct?

keel can record a project convention from one session and surface it automatically on the next. To test whether that helps, we measured rule compliance with and without the retrieved convention. Solver claude-opus-4-8, judge claude-sonnet-5, 4 trials per case.

| test set | without keel | with keel | gain |
|---|---|---|---|
| synthetic, invented rules | 0% | 95% | +95 |
| vs code, typescript | 73% | 94% | +20 |
| django, python | 62% | 92% | +29 |
| prometheus, go | 58% | 90% | +31 |
| all real repos, 3 languages | 66% | 92% | +26 |

Control: give the model a wrong convention instead of the right one. It scores the same as giving nothing, a 0% gain. So the gain comes from the specific rule, not from adding more text to the prompt.

The effect holds across three languages. The less a model already knows a language's conventions, the larger the gain.

### Read speed

`keel status` on the linux kernel, 80,000 files, median of 5 runs.

| | time |
|---|---|
| keel status, daemon running | under 10ms |
| keel status, no daemon | 0.42s |
| git status | 0.26s |

The daemon keeps a warm index and does work proportional to what changed, not to repo size. git is faster with no daemon. keel is faster with one.

## keel vs git

What keel adds.

- One fetch returns the full context for a task. The relevant code, the dependency graph, who wrote what, and past sessions. git returns diffs.
- Memory across sessions. A convention recorded once is surfaced automatically later. Numbers above. git has none.
- Signed authorship. Every change traces to a human, agent work included. git records an author name, unsigned by default.
- Content-addressed storage. Every object has a hash and verifies against it. BLAKE3, chunked with FastCDC, delta compressed.

Where git wins.

- Shallow clones are faster.
- Smaller on disk. keel repack cuts its own size by 36% on a 300-commit import, and git is still smaller.
- 20 years of tooling and near-universal support. keel is new.
- Simpler for human-only work, where no agent reads history.

## Benchmark your own repo

The test: take rules your repo enforces, record each one, and measure whether retrieval makes an agent follow it.

```
# build keel
cd src/rust && cargo build --release

# check the harnesses run. no API calls. free.
cd ../keel-bench && python3 run_suite.py --dry-run

# run a published benchmark. needs an Anthropic key in ~/.claude-token.
TRIALS=4 python3 flywheel_bench.py

# your repo: copy a harness, point it at your checkout,
# replace the rule list with rules your repo enforces.
cp corpus_bench_django.py corpus_bench_mine.py
# edit the scenario list to (file, description, task, rule, check), then:
CORPUS_SRC=/path/to/your/repo python3 corpus_bench_mine.py
```

Each scenario is one rule: the file it applies to, a task, the rule itself, and the check the judge uses. keel learns the rule, retrieves it, the agent solves the task with and without it, the judge scores compliance. Details in `src/keel-bench/SUITE-RESULTS.md`.

## Repo layout

Rust workspace under `src/rust`.

- keel-store · object store. content-addressed, BLAKE3, FastCDC chunking, delta compression, LMDB.
- keel-resolve · language resolvers. builds import and symbol graphs per language.
- keel-graph · the live dependency graph, kept warm.
- keel-brief · assembles the per-task context in one fetch.
- keel-coord · coordination. reservations and conflict prediction across sessions.
- keel-git · git compatibility. byte-identical objects, packfiles, a smart-HTTP server, two-way mirror.
- keel-net · transport over QUIC. fetch objects by hash, stream live events.
- keel-daemon · keeld. keeps the store, graph, and status warm so reads are fast.
- keel-cmd · the `keel` binary. a drop-in for git.
- keel-core · shared types.

Benchmarks are in `src/keel-bench`. Design notes are in `src/docs`.

## Build

```
cd src/rust
cargo build --release    # builds target/release/keel and target/release/keeld
cargo test --release     # 20 test suites
```

## Status

keel is early. Verified today.

- git compatibility. clone, fetch, pull, push. a git push lands as a native commit. objects are byte-identical across 47,000 real ones.
- status and diff match git, including gitignore and symlinks.
- the correctness gain, benchmarked above.
- the daemon. warm context, fast status, coordination over QUIC.

Not yet. on-disk size matching git. hosted multi-repo serving, which is a separate project called hull. a frozen on-disk format.

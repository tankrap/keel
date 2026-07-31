# keel

keel is version control for AI-written code. It is git-compatible, so a plain `git push` to a keel repo lands as a normal keel commit, and clone, fetch, pull, and push all work unchanged.

The difference from git is what a read gives you back. keel treats each agent session as a first-class piece of history, and a single fetch returns everything an agent needs for a task: the relevant code, the dependency graph, a signed record of who wrote what, and lessons from past sessions. git returns diffs and leaves you to assemble the rest.

## Benchmarks that matter

The numbers below are measured, with 95% confidence intervals, and you can reproduce them from `src/keel-bench`.

### Does it make agents more correct?

keel can record a project convention from one session and surface it automatically on the next. To test whether that actually helps, we measured how often a model followed a rule with and without the retrieved convention. The solver was claude-opus-4-8, the judge was claude-sonnet-5, and each case ran four trials.

| test set | without keel | with keel | gain |
|---|---|---|---|
| synthetic, invented rules | 0% | 95% | +95 |
| vs code, typescript | 73% | 94% | +20 |
| django, python | 62% | 92% | +29 |
| prometheus, go | 58% | 90% | +31 |
| all real repos, 3 languages | 66% | 92% | +26 |

As a control, we gave the model a wrong convention instead of the right one. It did no better than with nothing at all, a 0% gain, which means the improvement comes from the specific rule and not from simply adding text to the prompt.

The effect holds across three languages, and it grows as the model knows less about a language's conventions to begin with.

### Read speed

This is `keel status` on the linux kernel, 80,000 files, as the median of five runs.

| | time |
|---|---|
| keel status, daemon running | under 10ms |
| keel status, no daemon | 0.42s |
| git status | 0.26s |

With the daemon running, keel keeps a warm index and does work proportional to what changed rather than to the size of the repo. Without it, git is faster. With it, keel is.

## keel vs git

keel adds four things git does not have.

- A single fetch returns the full context for a task: the relevant code, the dependency graph, who wrote what, and past sessions. git returns diffs.
- Memory across sessions. A convention recorded once is surfaced automatically later, with the gains shown above. git has no equivalent.
- Signed authorship, so every change traces back to a human, agent work included. git records an author name that is unsigned by default.
- Content-addressed storage, where every object has a hash and verifies against it. It uses BLAKE3, chunks with FastCDC, and delta-compresses.

git still wins in four places.

- Shallow clones are faster.
- git repos are smaller on disk. keel's repack cuts its own size by 36% on a 300-commit import, and git is still smaller than that.
- git has twenty years of tooling and near-universal support, and keel is new.
- For human-only work, where no agent ever reads the history, git is simpler.

## Benchmark your own repo

The idea is to take rules your repo enforces, record each one, and measure whether retrieval makes an agent follow it.

```
# build keel
cd src/rust && cargo build --release

# check the harnesses run, with no API calls, for free
cd ../keel-bench && python3 run_suite.py --dry-run

# run a published benchmark (needs an Anthropic key in ~/.claude-token)
TRIALS=4 python3 flywheel_bench.py

# for your repo, copy a harness, point it at your checkout,
# and replace the rule list with rules your repo enforces
cp corpus_bench_django.py corpus_bench_mine.py
# edit the scenario list to (file, description, task, rule, check), then run:
CORPUS_SRC=/path/to/your/repo python3 corpus_bench_mine.py
```

Each scenario is a single rule, described by the file it applies to, a task, the rule itself, and the check the judge uses. keel learns the rule and retrieves it, the agent solves the task with and without it, and the judge scores whether the result complies. There is more detail in `src/keel-bench/SUITE-RESULTS.md`.

## Repo layout

The Rust workspace lives under `src/rust`.

- keel-store · the object store: content-addressed, BLAKE3, FastCDC chunking, delta compression, on LMDB.
- keel-resolve · language resolvers that build import and symbol graphs per language.
- keel-graph · the live dependency graph, kept warm.
- keel-brief · assembles the per-task context in one fetch.
- keel-coord · coordination, including reservations and conflict prediction across sessions.
- keel-git · git compatibility: byte-identical objects, packfiles, a smart-HTTP server, and a two-way mirror.
- keel-net · transport over QUIC, for fetching objects by hash and streaming live events.
- keel-daemon · keeld, which keeps the store, graph, and status warm so reads stay fast.
- keel-cmd · the `keel` binary, a drop-in for git.
- keel-core · shared types.

Benchmarks live in `src/keel-bench`, and design notes in `src/docs`.

## Build

```
cd src/rust
cargo build --release    # builds target/release/keel and target/release/keeld
cargo test --release     # 20 test suites
```

## Status

keel is early, but the core works. What is verified today: full git compatibility, so clone, fetch, pull, and push all work and a git push lands as a native commit, with objects byte-identical across 47,000 real ones. status and diff match git, including gitignore and symlinks. The correctness gains above reproduce. The daemon serves warm context and fast status, and coordinates over QUIC.

What is not there yet: on-disk size does not match git, there is no hosted multi-repo serving (that is a separate project, hull), and the on-disk format is not yet frozen.

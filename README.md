# keel

keel is version control for AI-written code. It is git-compatible, so a plain `git push` to a keel repo lands as a normal keel commit, and clone, fetch, pull, and push all work unchanged.

git records what changed: a commit, a message, and a diff, and leaves the rest for you to reconstruct. keel records the work session that produced the change, and keeps git underneath as the compatibility layer.

The difference shows up on read. git hands you a diff. keel answers a different question, what an agent needs to do the next task, and returns it in one fetch: the relevant code, the dependency graph, a record of who wrote what, and the lessons earlier sessions learned in this repo. git leaves you to assemble that from diff, blame, and log.

## Benchmarks that matter

Everything here is measured on real repositories and reproducible from `src/keel-bench`. Full numbers, with 95% confidence intervals, are in [SUITE-RESULTS.md](src/keel-bench/SUITE-RESULTS.md).

### Does it make agents more correct?

keel records a convention discovered in one session and surfaces it automatically on the next related task. To test whether that helps, we measured how often a model follows a project's own rule with and without the retrieved convention. Solver claude-opus-4-8, judge claude-sonnet-5, four trials per case.

```text
how often an agent follows the project's own convention
   ░ the model on its own        █ what keel's retrieved lesson adds

synthetic   ██████████████████████████████████████     0 → 95   +95
vs code     ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░█████████    73 → 94   +20
django      ░░░░░░░░░░░░░░░░░░░░░░░░░████████████     62 → 92   +29
prometheus  ░░░░░░░░░░░░░░░░░░░░░░░█████████████      58 → 90   +31
tokio       ░░░░░░░░░░░░░░░░░░░░░░░░███████████████   60 → 98   +38
──────────────────────────────────────────────────────────────────
pooled      ░░░░░░░░░░░░░░░░░░░░░░░░░░███████████     64 → 93   +29
```

Pooled across four languages, retrieval takes an agent from 64% to 93%, a +29 point gain. It concentrates on the conventions a model has not already memorized, so the largest lift is on tokio (Rust, +38), whose codebase leans hard on repo-local idioms a general model rarely internalizes.

To rule out "any authoritative-looking text helps," a control retrieves a deliberately wrong convention instead of the right one. It adds nothing, so the gain is the specific rule and not extra tokens in the prompt.

```text
adversarial control, synthetic scenarios
no lesson                                                0%   baseline
correct rule  ██████████████████████████████████████    95%   +95
a wrong rule                                             0%   +0
```

Every convention was retrieved: 72 of 72 across the real corpora, 20 of 20 synthetic.

### Does it pull the right context?

A brief is only useful if it holds what the task actually needs and stays small enough to hand a model. On a 100-symbol sample of VS Code, `keel brief` pulled in the correct cross-file definitions 97% of the time, at a median of about 310 tokens per brief, small enough to drop into a prompt without spending the context window.

### Read speed

`keel status` on the linux kernel, 80,000 files, median of five runs:

| | time |
|---|---|
| keel status, daemon running | under 10ms |
| keel status, no daemon | 0.42s |
| git status | 0.26s |

With the daemon running, keel keeps a warm index and does work proportional to what changed rather than to the size of the repo. Without it, git is faster. With it, keel is.

### Holds up at scale

The store and the dependency graph on the full linux kernel, single machine (Apple Silicon, release build), directional:

| | |
|---|---|
| full ingest, 94,500 files | 37.5s |
| on-disk store, share of the input size | 71% |
| dependency graph, 337,000 edges | 25.7s |

## keel vs git

git was built for people sending each other patches. Its unit is the diff, and everything under it exists to move diffs between humans. That model has nothing to say about what now writes most of the code: an agent that has to rebuild the full context of a codebase before it can make a single correct change.

keel is built for that reader. It records each work session as a first-class object in the history, which turns version control from a log of diffs into a source of context and memory. Four differences follow from that, and none of them is something git can be extended into.

- **Context in one read.** A single fetch returns everything an agent needs for a task: the relevant code, the dependency graph, who wrote what, and the sessions that touched it. In git you assemble that yourself, from diff, blame, and log, across many commands.
- **Memory across sessions.** A convention learned in one session is recorded and surfaced automatically on the next related task. In the benchmark above that raises how often an agent follows a real project rule from 64% to 93% across four languages, and the control run shows the gain is the specific rule, not extra prompt text. git has no equivalent, so an agent starts every task cold.
- **Sessions as history.** keel records how a change was made, the task, the model, the prompts, the tool calls, and whether the result verified, as an object you can query later. git records the commit and an author name, and nothing about how the change was produced.
- **Reviews are objects too.** A review of a session is stored in the repo with its verdict, labels, and findings, so you can query across reviews: every security review, every disagreement, every change approved without a human. git keeps reviews outside the repository, so it cannot answer those.
- **Review the change, not the diff.** Because the session and the verdict are already recorded, `keel walkthrough <change>` narrates a change block by block — what the agent was asked to do, and, for each file, the proof that backs it: a test file is its own evidence, a source file is linked to the tests that changed alongside it, and an untested block that never went green is flagged for a closer look. It is derived from the recorded work, not re-generated after the fact, so it is a lookup, not another model in the loop. git has no session or verdict to narrate from, so its review is always "read every line and hope you catch the one that matters."

The second of these is the point. A convention learned once and applied automatically is version control that gets better at your codebase as it is used, and the benchmark measures one turn of that loop. That is a different data model, not a git extension.

### Where git wins

keel is new, and git is not going anywhere for a lot of work.

- Shallow clones are faster.
- git repos are smaller on disk. keel's repack cuts its own size by 36% on a 300-commit import, and git is still smaller than that.
- git has twenty years of tooling and near-universal support.
- For human-only work, where no agent ever reads the history, git is simpler, and you need none of the above.

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

Benchmarks live in `src/keel-bench`, and design notes in `src/docs`, including the [positioning](src/docs/positioning.md) for why keel exists and what is shipped versus planned.

## Build

```
cd src/rust
cargo build --release    # builds target/release/keel and target/release/keeld
cargo test --release     # 20 test suites
```

## Status

keel is early, but the core works. What is verified today: full git compatibility, so clone, fetch, pull, and push all work and a git push lands as a native commit, with objects byte-identical across 47,000 real ones. status and diff match git, including gitignore and symlinks. The correctness gains above reproduce. The daemon serves warm context and fast status, and coordinates over QUIC.

What is not there yet: on-disk size does not match git, there is no hosted multi-repo serving (that is a separate project, hull), and the on-disk format is not yet frozen.

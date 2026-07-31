# keel positioning

*Forward-looking positioning, not a capability list. The shipped parts below are factual; the rest is marked as direction. For exactly what is built and measured today, see the [README](../../README.md) and the [benchmark results](../keel-bench/SUITE-RESULTS.md).*

## The premise

git was designed for people. keel is designed for agents. That sounds like a small distinction, but it changes most of the assumptions underneath version control.

git stores code and assumes a human supplies everything else: the architecture, the conventions, the coordination, the intent, the context an engineer reconstructs before every change. That held while people were the primary readers and writers of software. As agents become primary contributors it stops holding, because an agent does not carry that context between tasks. The information a human kept in their head has to become part of the system.

## The shift

git optimizes persistence. It answers one question well, what changed. keel optimizes for the next task. It answers a different one, what an agent needs to do the next piece of work correctly. That second question changes the read model, not just the storage.

## The data model

git stores commits, trees, and blobs, and reconstructs everything else from them. The commit is the primary object and history is a sequence of commits.

keel keeps git's objects for compatibility but promotes other things to first-class: the work session, the assembled context, learned conventions, the dependency graph, coordination state. The commit becomes an interoperability layer rather than the main abstraction. The internal model changes and git compatibility stays.

## Sessions, not just commits

git records an author, a timestamp, a message, and a diff. keel records the work session that produced the change.

Today a keel session stores the task, the model, the learned convention, references to the prompts and tool calls and their results, and the verification outcome. The direction is to capture more of what a session actually is: its reasoning, the semantic shape of the change, CI results, and dependency impact. The point is the same either way. The session becomes the unit of engineering, and the commit becomes one artifact it produces. *(Shipped: the session object and the fields above. Building: the fuller record.)*

## The read model

An engineer builds context by hand: checkout, diff, blame, log, grep, a language server, some dependency analysis, all combined in their head. keel returns it in one fetch: the relevant files, the dependency graph, earlier sessions, the project's own conventions, and who wrote what. The repository becomes queryable by intent rather than by file.

## System memory instead of human memory

People have been git's missing daemon. They avoid stepping on each other, remember the conventions, know which changes are risky, and talk before they collide. git never modeled any of it. It delegated all of it to humans. keel moves that responsibility into software.

## The daemon as an active runtime

keel runs a daemon, and it is not only a cache. It keeps the index and the dependency graph warm, assembles context, streams events, and moves data over its own transport. That turns version control from passive storage into an active runtime. On a warm daemon, status on an 80,000-file tree returns in under 10ms.

## Coordination before conflict, not after

git coordinates after the fact: two agents edit the same file, the merge conflicts, a human resolves it. Because the daemon knows what is in flight, keel can coordinate before the fact instead: an agent asks for work, the daemon knows what is owned, predicts the conflict, reserves the region, and the agent proceeds safely. *(Building: reservations and conflict prediction exist as a component. This is direction, not a matured capability.)*

## The flywheel

This is the part that is real and measured. Every session discovers something: a convention, a constraint, a reason a thing is done a certain way. Most systems lose it. keel records it and retrieves it for the next related task, so the next session starts warmer than the last and the repository gets better at its own codebase as it is used.

It measurably improves correctness. On real codebases, retrieving a learned convention raises how often an agent follows it from 64% to 93%, pooled across four languages (TypeScript, Python, Go, and Rust), and a control with a deliberately wrong convention gives no lift at all, so the gain is the specific rule and not extra text in the prompt. Full numbers are in [SUITE-RESULTS.md](../keel-bench/SUITE-RESULTS.md). *(Shipped and benchmarked.)*

## Review, reimagined *(vision)*

The hosted review flow in this section is direction, not built, and it is the goal a separate hosted project, hull, is aimed at. One foundational piece already exists in keel: reviews are first-class objects, so the cross-review queries below run today.

Today's review tools review a diff. A human reads it, and AI review tools add a model that reads the same diff first. The artifact is still the diff. If the session is first-class, the thing to review is the session, not the diff: a review package that carries the task, the reasoning, the semantic operations, tests and CI, dependency impact, and a risk read, so a human reviews synthesized understanding rather than raw text.

Two ideas follow from that. Review can be independent by construction, with a model from a different family than the one that wrote the code, to reduce correlated blind spots. The goal is independent critique, not consensus. And review itself is a first-class object, which keel implements today, so you can ask questions across reviews: every security review, every disagreement, everything that mentioned a race condition, everything approved without a human. Those queries run now via `keel reviews`. git has no representation for any of that.

## Semantic operations, not line diffs *(vision)*

People do not think in line diffs. They think in operations: added authentication, introduced optimistic locking, split the cache layer, added a retry. A review surface should show the operation, not only the text. This depends on semantic diff, which is roadmapped, not shipped.

## Positioning

git stores what changed. keel stores what the next engineer, or agent, needs to know.

git is distributed version control. keel is AI-native source control that stays git-compatible.

The core of it: keel is not trying to replace git's interoperability. It is replacing git's assumption that a human is the runtime. Once agents are the runtime, version control becomes responsible not only for persistence but for memory, coordination, context, and review.

## Where this stands today

| area | status |
|---|---|
| git compatibility: clone, fetch, pull, push, byte-identical objects, two-way mirror | shipped |
| session as a first-class object | shipped (core fields); fuller record building |
| one-fetch context assembly: code, graph, authorship, past sessions, conventions | shipped |
| warm daemon: status, graph, context, events, transport | shipped |
| the flywheel: learn, retrieve, measured correctness lift | shipped and benchmarked |
| coordinate-before-conflict: reservations, conflict prediction | building |
| semantic diff and semantic operations | roadmapped |
| reviews as first-class objects, with cross-review queries (`keel review` / `keel reviews`) | shipped |
| session-based review packages, independent-model review, semantic review | vision (hull) |
| cryptographic authorship and human accountability | vision (hull) |

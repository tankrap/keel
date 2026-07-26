# Decision 0001: substrate — staged adoption, porcelain first

Status: **decided** (2026-07-26) · resolves keel#9

## Question

Build keel's v0 on Jujutsu as a substrate, or start a greenfield core?

## Decision

**Both, staged — and neither first.** The v0 ships as a thin, zero-dependency
porcelain over **git** itself; jj becomes the substrate when the change model
lands; the greenfield core comes last, when benchmarks justify it.

1. **v0 (now): porcelain over git.** A single-file CLI (`src/keel.mjs`) that
   implements the token-economy surface — `st`, `d`, `save`, `sync`, `log`,
   `fix`, `undo` — against any existing git repo.
2. **v0.2: jj substrate behind the same verbs.** Working-copy-as-commit, stable
   change IDs, first-class conflicts, op log — jj has already proven all of it,
   including bidirectional git interop. keel detects a jj repo and upgrades its
   semantics; the command surface does not change.
3. **v1: greenfield core.** BLAKE3 + FastCDC single-database store, the daemon,
   and the wire protocol (keel-server). Built only once keel-bench shows the
   porcelain's ceiling — and reusing its measured behavior as the spec.

## Why porcelain-over-git first

- **The thesis is the token economy, and it is testable today.** Every claim in
  the design (digest-with-handles, budgets, structured errors, one-call save)
  can be proven against the VCS the whole world already runs, with keel-bench
  measuring git-vs-keel on day one. No storage engine is on that critical path.
- **Reliability of the client.** Agents emit git-shaped commands with maximal
  fidelity; keel v0 stays inside that distribution while collapsing round trips.
- **Zero adoption cost.** `node keel.mjs` in any repo. No migration, no daemon,
  no server. The fastest possible loop for dogfooding keel on keel.
- **jj is a better substrate than a foundation to fork now.** Its change model
  is exactly what we want (design §2 adopts it verbatim), but building v0 *on*
  jj forces every early user (and benchmark baseline) through a jj install and
  colocated-repo setup before the token thesis is even demonstrated.

## What would change the decision

- keel-bench showing git's plumbing itself (status/diff latency on large repos)
  dominating the agent loop → accelerate the jj/core stages.
- jj-lib stabilizing a Rust API we can embed cleanly → v0.2 may skip the
  jj-CLI-wrapping step and embed directly.

## Consequences

- `src/keel.mjs` treats the backend as an interface: `git` today, `jj` next,
  `core` last. Nothing in the output contract may leak the backend.
- The daemon/workspace work (keel#6, #19) targets the jj stage, not git.
- keel-server's chunk store (keel-server#2) is unblocked but not urgent; the
  protocol *draft* (keel-server#7) proceeds now so the core has a spec to grow
  into.

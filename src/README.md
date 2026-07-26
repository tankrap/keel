# keel

**A version control system designed for AI agents first, humans always.**

> Working codename. A keel is the backbone of a hull — the part every hallmark rests on.

Git was built for 2005-era terminal scripting: verbosity is free, interactivity is
assumed, errors are prose. Agents invert every one of those economics — tokens are
metered in both directions, interactivity is impossible, and every round trip
re-frames the whole exchange. keel is a VCS built for that client, without giving
up anything humans need.

## Hallmarks

- **Speed** — BLAKE3 hashing, content-defined chunking, single-database storage
  (no loose objects, no `index.lock`, no gc), lazy materialization.
- **Token economy** — intent-level commands (`save`, `sync`, `st`, `d`), digest
  output with drill-down handles, token budgets as a first-class contract,
  structured errors with remediation, session-cursor deltas.
- **Agent-native** — zero interactivity, idempotent commands, machine-readable
  everything, per-session attenuated credentials, provenance built in.
- **Human-usable** — TTY detection renders the same commands rich for people;
  full hydration restores the classic everything-local experience; every summary
  has a one-step path to raw bytes.
- **Modern change model** — working-copy-as-commit, stable change IDs, first-class
  conflicts (no wedged rebases), operation log with universal undo (Jujutsu's
  model, adopted deliberately).
- **Git feature parity via interop** — speaks the git wire protocol both ways
  during adoption.

## v0 is real

`src/keel.mjs` — single-file, zero-dependency Node ≥22 CLI over the git backend
(substrate staging: [decisions/0001](decisions/0001-substrate.md)). In any git repo:

```
node src/keel.mjs st              # whole situation, one call; "=" when unchanged
node src/keel.mjs d --usage       # digest diff + counterfactual token frame
node src/keel.mjs d a.js --budget 500   # hunks for one file, budget-capped
node src/keel.mjs save "msg"      # snapshot + describe → {"id":"abc1234"}
node src/keel.mjs sync            # pull+push; conflicts come back structured
```

Piped output is stable-key JSON; errors are `{error, message, fix}` + exit 1;
never interactive. Tests: `node --test tests/cli.test.mjs`.

## Repos

| repo | contents |
|---|---|
| `justin_harris/keel` | this repo — the CLI (+ per-machine daemon) |
| `justin_harris/keel-server` | server: wire protocol, server-side queries, identity |
| `justin_harris/keel-bench` | benchmarks: token benchmarks, perf suite, chaos/durability |

## Design

**[docs/handbook.md](docs/handbook.md)** explains how everything works;
[design.md](design.md) holds the founding design; `keel-bench:src/NUMBERS.md`
is the evidence pack. Nothing here is real until `keel-bench` says it is —
and it now says **60–75% fewer tokens per task, measured and attested**.

> Note: seed docs live under `src/` because the default bsmnt agent grant is
> path-scoped to `src/**, tests/**` — repo-root files are unpushable by agents
> today (platform issue filed). Move to the root once that lands.

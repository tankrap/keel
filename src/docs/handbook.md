# The keel handbook

How keel works, end to end — first draft (2026-07-26). Design rationale lives
in [../design.md](../design.md); measured evidence in `keel-bench:src/NUMBERS.md`.
This document explains what exists and how to use it.

## 1. What keel is

A version control system built for AI agents first, humans always. Git's
interface assumes a human at a 2005 terminal: verbosity is free, interactivity
is available, errors are prose. Agents invert all of that — every token is
metered, interactivity is impossible, and each round trip re-frames the whole
exchange. keel keeps git's proven data model underneath (and jj's change model
where available) and rebuilds the *interface economics*: **60–75% fewer tokens
read per task, with fewer round trips** (measured; see NUMBERS.md).

Four components, four repos:

| component | repo | what it is |
|---|---|---|
| CLI + daemon | `keel` | `src/keel.mjs` (the tool), `src/keeld.mjs` (per-machine daemon) |
| server | `keel-server` | `src/server.mjs` (sync + queries), `src/chain.mjs` (identity) |
| benchmarks | `keel-bench` | token/perf/chaos suites, recordings, attestations |
| lab | `keel-lab` | hermetic environments, attest/verify, merge-triggered CI |

Everything is zero-dependency Node ≥22, single-file per component, on purpose:
auditable, portable, no supply chain.

## 2. The output contract (why it saves tokens)

Every keel command obeys one contract:

- **Piped output is stable-key JSON**; a TTY gets readable formatting. Same
  data either way — the renderer, not the content, adapts.
- **Lists are header-once columnar**: `{"cols":["p","+","-","fns"],"files":[[…]]}`
  — the schema is paid once, not per row.
- **Errors are actions**: `{"error":"E_CONFLICT","message":…,"fix":"edit the
  files, then: fix --continue"}` + exit 1. The agent acts instead of diagnosing.
- **Digest by default, drill-down by name**: `d` returns per-file counts and
  function-level change tags (`normalize:new util_0_0:gone`); `d <path>` or
  `--full` returns hunks, capped by `--budget N` (tokens) with *explicit,
  expandable* elision — never silent truncation.
- **Nothing is re-sent**: `st` prints literally `=` when nothing changed since
  the last look; `d` returns `{seen}` markers for content already shown
  (`--reshow` overrides). Byte-stable ordering keeps prompt caches warm.
- **Success is an id, not a paragraph**: `save` returns `{"id":"…"}`.

## 3. The daily loop (CLI)

```
keel st                    # whole situation: branch/change, files ±, conflicts, overlap
keel d [path] [--budget N] # digest diff; hunks by path; --usage adds counterfactual
keel save "msg"            # snapshot everything + describe (no staging exists)
keel sync [--rebase]       # pull+push via linked server, or git remote (--git)
keel fix [--continue|--abort]  # conflicts as data; resume/abort
keel log [-n N]            # compact history
keel undo                  # revert the last operation
keel profile / metrics     # effective config with sources / token accounting
```

Backends (decisions/0001, staged): plain **git** repo → git plumbing.
**jj-colocated** repo (`jj git init --colocate`) → `save` becomes a jj commit
returning a **stable change id** that survives rewrites, `undo` becomes jj's
op-log undo (reverses *any* operation), `log` reads change ids, `st` shows the
working-copy change. The output contract is identical — the backend never leaks.

Profiles (`~/.keel/profile.json`, `KEEL_PROFILE=agent|human`, `KEEL_BUDGET`,
`KEEL_RENDER`) set budget/renderer/cursor per machine; `keel profile` prints
every effective value with its source. Every response appends a usage frame to
`.git/keel/metrics.jsonl` including the full-dump counterfactual; `keel
metrics` reports tokens spent and tokens *displaced*.

## 4. Sync: the server path

```
keel link http://host:port myrepo    # enroll this machine (Ed25519 keypair)
keel push                            # bundle → sha256-verified chunk → signed ff-push
keel pull                            # event-discovered chunk → verified → ff-only
keel clone http://host:port myrepo   # init + enroll + pull, from nothing
```

`sync` prefers a linked server automatically. Pushes are attributed and
idempotent (same idempotency key → original result, even after a server
crash); pulls verify chunk hashes before touching the repo and refuse
divergence with a structured remediation. Chunk fetches route through the
daemon's shared cache when it's running.

The server (`node src/server.mjs --port N --store dir`) implements
protocol-v0.md over HTTP/1.1+JSON (the RPC semantics are the contract; QUIC is
the planned transport): content-addressed verified chunks, ff-only pushes (no
force-push exists), seq-numbered events with cursor replay, and **zero-checkout
queries** — after a push with `ingest:true`, `GET /v0/q/log|skeleton|diff`
answer about the repo without any client materializing it.

## 5. Identity: the chain

`org → account → machine → session`, as nested Ed25519 certificates — each
cert embeds its issuer's, so a session credential carries its whole ancestry.
Verification enforces: trusted org root, valid signature per link, tier order,
**attenuation-only** (a child can never broaden ref scope or extend expiry),
TTL, and revocation at any tier — revoking a machine instantly kills every
session under it, and the revocation lands on the event stream.

```
keel id init                          # mint org→account→machine into ~/.keel/id
keel id mint --refs proj/* --ttl 3600 # scoped session credential for this repo
keel id revoke <cert-id>              # org-signed revocation at the server
```

`push` automatically uses the strongest credential present (session chain >
flat machine key). Every act is attributed to the full chain:
`org:…/account:…/machine:…/session:…`. Verification is the tool's job — trust
costs the agent zero tokens.

## 6. The daemon (multi-agent machines)

`node src/keeld.mjs` — one per machine, crash-only by design: there is no
shutdown path; every mutation is journaled before it's acknowledged, and boot
*is* recovery (kill -9 is tested, not feared). JSON-lines over a unix socket:

- **`acquire {session, base, refs?, ttl?}`** → a workspace (git worktree —
  all workspaces share one object store) **plus a scoped session credential
  minted under the machine's chain, in one operation**. `release` cleans up.
- **overlap detection**: `st` reports each session's working set; the daemon
  warns any session touching a file another session is editing — a ~15-token
  warning instead of a wasted iteration.
- **shared verified chunk cache**, single-flighted: N agents fetching the same
  chunk hit upstream exactly once.
- `keel fleet` shows every session on the machine.

## 7. Benchmarks you can check (keel-bench + keel-lab)

Three evidence tiers (full data: `keel-bench:src/NUMBERS.md`):
recorded live sessions (n=3 aggregate **69.7%** savings), scripted scenarios
(69–75% vs git, 64–70% vs jj, attested), and context (jj alone ≈15–18%; keel
v0 latency is *worse* than git — documented porcelain tax).

What makes them defensible:

- **Hermetic environments** (keel-lab): every run is stamped with a hash of
  its environment manifest; `lab compare` refuses mismatched stamps. Docker
  backend runs sealed (`--network none`, pinned image by digest, fixed CPU).
- **Attestation**: `lab attest` signs results + exact source commits;
  `lab verify` re-runs the suite and demands **exact equality** (token counts
  are deterministic — proven across OSes and node versions).
- **Continuous verification**: a launchd agent re-runs suites → gate → e2e →
  attest → verify on every merge and posts the signed result to the forge
  (keel-lab issue #6). Regressions fail loudly and file issues.
- **The system test**: `e2e.mjs` — 19 steps covering verified transfer,
  event-driven sync, overlap warning, chain-attributed pushes, zero-checkout
  queries, and kill -9 recovery of both server and daemon. Runs locally or
  fully containerized.

## 8. Honest limits (v0)

Per-call latency is 5–10× git's (Node wrapper; the daemon/core stages erase
it). Transport is HTTP/1.1, not QUIC. Org trust is TOFU, channel binding needs
TLS. Recorded-session n is small. The chunk store holds whole bundles (FastCDC
and BLAKE3 arrive with the core store). File-level CoW awaits the same. None
of these are hidden — each is an open issue or a design-doc stage.

## 9. Try it in five minutes

```
# terminal 1: a server
node keel-server/src/server.mjs --port 7777 --store /tmp/ks

# terminal 2: two "agents"
node keel/src/keel.mjs id init
cd /tmp && mkdir a && cd a && git init -q -b main
keel link http://127.0.0.1:7777 demo
echo hi > x.txt && keel save "first" && keel push
cd /tmp && keel clone http://127.0.0.1:7777 demo b   # second agent, same history
```

Then kill -9 the server, restart it on the same store, and push again — the
idempotent replay answers as if nothing happened. That's the design in one
gesture: verifiable, structured, and unkillable-by-accident.

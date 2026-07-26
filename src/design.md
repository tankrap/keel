# keel design

Status: draft v0 — captured from the founding design discussion, 2026-07-26.
This document is the source of truth for scope; the issue tracker holds the work
breakdown across `keel` (CLI + daemon), `keel-server`, and `keel-bench`.

## Thesis

Git's client is a human at a terminal in 2005. keel's client is an AI agent with
a metered context window — and also a human, at full fidelity, whenever they
want. The design goal is not "terse"; it is **right-sized information per call**:
digests with handles for metered readers, raw bytes one step away for everyone.

Trust and token-saving are the same feature: an agent can only afford to *not
read* content when cryptography makes not-reading safe. Provenance is a
context-window optimization.

## 1. Data model

- **Merkle DAG** over content-addressed objects (keep git's foundation).
- **BLAKE3** everywhere — parallel, fast, verified streaming (chunks validated
  as they arrive).
- **Content-defined chunking (FastCDC)** at the blob layer: large-file support
  and cross-repo dedup are native, LFS does not exist.
- **Single-database store** (SQLite/LSM-class, WAL, MVCC) per machine — no loose
  objects, no packs, no gc, no lock files. `status` is an indexed query.

## 2. Change model (Jujutsu's, adopted deliberately)

- Working copy **is a commit**; no staging area, no stash, no dirty-tree errors.
- **Stable change IDs** survive rebase/amend — agent references never dangle.
- **First-class conflicts**: merge/rebase always succeed; conflicts are recorded
  in the commit and resolved as ordinary edits. No `--continue`/`--abort` state
  machine, no wedged working trees.
- **Operation log** journals every mutation (not just commits); `undo` reverses
  anything. Crash recovery and offline queueing ride the same log.

## 3. Command surface (CLI)

Intent-level verbs, single common words, near-zero flags, never interactive,
always idempotent:

```
st                 whole situation in one call: change, files, conflicts, sync state
d [path[:fn]]      budgeted semantic diff; drill down by handle; batch handles OK
save "msg"         snapshot everything + describe (replaces add+commit)
sync               pull + rebase + push; conflicts land as data, never wedge
log <query>        revset query with field selection, one shot
fix                list conflicts / mark resolved
undo               revert last operation, whatever it was
get / hydrate      materialize files / deepen a lazy checkout
profile            show/set the effective profile (see §6)
```

Terse-but-familiar: models must emit these reliably, so syntax stays inside the
distribution of CLIs they know. A failed cryptic call costs more than the tokens
it saves.

## 4. Output formats and token economy

- **TTY detection**: humans get color/pager/prose; piped output is structured.
- **Pay schema once**: header-once columnar records for lists; JSON only for
  genuinely nested data. Raw 32-byte hashes rendered as short handles.
- **Digest by default, drill down by handle**: `d` returns file/function-level
  semantics (`validateScope(changed)`); full hunks on request; batch expansion in
  one round trip. Small diffs below budget return whole — no elision tax on the
  common case.
- **`--budget N` is a contract**: response fits, elision is explicit and
  expandable (`…+42 more (log @in)`), never silent.
- **Session cursor**: repeated looks cost ~1 token when nothing changed (`=`);
  sync output is ranked by intersection with the session's working set; the
  cursor tracks *what the agent has been shown* so upstream changes arrive as
  deltas against what is already in context.
- **Errors are code + remediation**: `!E_CONFLICT … → edit files, then: save`.
  Every profile-caused limitation names the knob and the two ways out.
- **Byte-stable output**: sorted ordering, no timestamps by default — prompt
  caches stay warm.
- **Suppress the echo**: success returns an ID, not a paragraph.

## 5. Trust = elision license

- CLI verifies signatures/Merkle linkage locally and reports `✓` — verification
  is the tool's job, never the model's.
- Every digest carries the hash of what it summarizes; any level spot-checkable.
- Elision is **versioned policy** (lockfiles, generated, binaries), never
  judgment — absence is informative, so agents don't defensively re-fetch.
- Harness-level sampling (~1%) keeps summaries honest.
- Trust-weighted review: review depth (token spend) priced by the provenance
  chain of the change.

## 6. Profiles: per-machine, not per-repo

The repo describes what it **is**; the machine declares how it **consumes**.
Profile groups: hydration (lazy/full, pins, blob ceiling, disk budget), output
(renderer, budget, elision), sync (prefetch, cadence), query routing, identity
(key, attenuation defaults), safety rails (gated rewrites). Presets: `laptop`,
`workstation`, `ci`, `agent`, `review`. Precedence: session > machine > org
preset > repo *hints* > defaults. `profile show` prints every effective value
with its source. Repo never dictates consumption; credentials, not claimed
profiles, are the enforcement boundary.

## 7. Daemon (multi-agent machine)

One daemon per machine owns the store, the network connection, and the indexes;
agents get **workspaces** (own working-copy commit + op log) over a local socket.

- Shared store: N agents ≈ one repo's storage + edits (reflink/CoW
  materialization); fetches single-flighted; caches (skeletons, ASTs, digests)
  shared.
- Per-session **attenuated credentials** minted at workspace acquire —
  branch-scoped, TTL'd; revoke one agent without touching the fleet.
- **Cross-agent awareness**: daemon sees all working sets → overlap warnings at
  edit time (a 15-token warning replaces a thousands-of-tokens wasted iteration);
  `fleet` gives the operator the whole swarm in one view.
- Integration happens at the server via merge queue — agents never rebase each
  other locally.

### Crash safety

Daemon is a **coordinator and cache, never the system of record**. Every
acknowledged mutation is WAL-durable; agent edits are real files on disk
regardless; memory-only state is rebuildable (caches, overlap index) or flushed
(session cursors — loss degrades to full re-send, never wrongness). Supervised,
spawned on demand, and **crash-only**: recovery is the boot path, exercised on
every start. The op log is the second parachute.

## 8. Sync protocol (see keel-server)

One QUIC connection per machine (TLS 1.3, 0-RTT resumption, connection
migration; HTTP/2 fallback): multiplexed unary RPC + verified chunk streams + a
server-push **event channel** (ref moves, merge-queue verdicts, credential
revocation, prefetch hints, cross-machine overlap). Frontier negotiation over
the commit graph; transfer at chunk granularity (anything ever fetched is never
re-sent); zstd with per-repo shared dictionaries; idempotency keys on every
mutation. Server is untrusted for content — everything client-verified. Server
also answers **queries** (diff, blame, search, symbol graph, skeleton reads) so
reading never requires materializing.

## 9. Identity: a delegation chain

```
org → account → machine → session (human shell | agent)
```

Machines are enrolled principals (mTLS cert, key in Secure Enclave/TPM where
available); sessions are ephemeral attenuations bound to the machine's channel.
Every act is attributable to the full chain; revocation has a blast radius at
every tier; metering rolls up the same chain. Enrollment must be cheap and
automatable (ephemeral certs for ephemeral machines) or people will share
identities and flatten the model.

## 10. Metrics (built into the protocol)

Usage frames per response; per-session/per-credential aggregation;
**counterfactual accounting** (tokens actual vs tokens a full dump would have
cost) as the headline savings metric and regression detector; profile-set token
budgets with daemon-enforced circuit breaking; OTLP/Prometheus export,
local-first.

## 11. Benchmarks gate everything (see keel-bench)

- **Token benchmarks**: replay recorded agent tasks against git vs keel —
  tokens-to-task-completion, round trips, success rate. CI-gated like latency.
- **Perf**: p50/p99 per verb, clone-to-first-edit, 1→128 workspace scaling, on
  linux/chromium/synthetic-monorepo corpora vs git/jj/Sapling baselines.
- **Chaos**: kill -9 at randomized points, fsync-failure/power-loss simulation,
  full disk, fuzzed merges. Invariant: nothing acknowledged is ever lost.

## 12. Interop and adoption

Bidirectional git interop (wire protocol + `.git` export) is non-negotiable for
adoption. Open build-vs-adopt question tracked as an issue: greenfield core vs
jj-as-substrate with keel as the agent-native porcelain + daemon + server. The
fast path to a usable v0 is likely jj-backed; the storage/protocol ambitions
eventually want the greenfield core.

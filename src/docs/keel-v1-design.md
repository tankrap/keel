# keel — Design Document (build handoff)

*Agent-native version control. The unit of history is the agent session, not the commit.*
Status: thesis benchmarked, stack decided, entering build. Draft 2026-07-28.
Companion to the Linear project **keel v1 — the fused code+session graph (build → benchmark)**.

---

## 0. How to use this document (read first, agents)

1. Read **§1 (The argument)** — it is the *why*. Every design choice descends from it.
2. Read **§2 (Architecture & stack)** — everything here is **decided**; do not relitigate without a decision issue.
3. Pick your task from the Linear project. **Every issue carries its benchmark bar and its harness.**
   Nothing ships below the number the prototype already hit (§6).
4. The two experiments in **§7 gate the strategy** — if you touch the flywheel or the moat claim, know them.

Non-negotiables: **build in Rust** (§2); **benchmark-gated** (§6); **honest numbers** — this project has
walked back every overclaim it ever made (see §1.7), keep that discipline.

---

## 1. The argument

### 1.1 The problem

Git was built for a few humans committing a few times a day. A fleet of AI agents breaks every assumption:

- **Context.** An agent needs the *right* code for its task. The full repo doesn't fit (~1M tokens for a
  real backend); `grep` retrieves the wrong slice (6–11% dependency recall on a real repo); reading a file
  misses cross-file dependencies. The answer to "login token expiry" lives two hops away in `auth.sign`,
  and only a call-graph follower finds it.
- **Coordination.** Git's model is "edit in parallel, merge later, resolve conflicts." For many agents on
  shared code that means wasted work and conflicts that could have been prevented.
- **Provenance.** A git commit stores a diff and a one-line message. *How* the change was made — the prompt,
  the context served, the tool calls, the model, whether it verified green — is discarded. For AI-written
  code that is the most important part.
- **Learning.** Nothing accumulates. The gotcha an agent discovered today is lost tomorrow; the next agent
  repeats the mistake.

### 1.2 The core idea: one live, fused graph

keel maintains a single **content-addressed graph of code + sessions**, live to the working tree, and
answers one fetch — the **brief** — that returns, for a task:

1. **Context** — exactly the code the task touches (its call/import subgraph).
2. **Coordination** — who else is editing what (reservations), on the same fetch.
3. **Provenance / verification** — which agent, what authority, is it green.
4. **Relevant prior sessions** — how this code was changed before, and what was learned.

### 1.3 Why it must be a VCS, not a layer — **transactional fusion** (the central argument)

This is the load-bearing argument. Read it twice.

A *layer beside git* can assemble the four pillars: a file-watcher gives liveness, a lock service gives
coordination, a store gives sessions, signing gives provenance. So why be a whole new VCS?

Because assembled that way, the four pillars are **four separate sources of truth, eventually-consistent
and racing under concurrency.** An agent acts on a reservation the lock service granted *while git already
conflicted*; it reads "live" state the watcher hasn't caught up on; it records a session that points at a
change the store hasn't durably written. At single-agent scale you never notice. At fleet scale these races
become **correctness bugs**.

keel makes the fused object — **change + session + verification + reservation — ONE atomic write in ONE
store.** They commit together or not at all, and can never be mutually inconsistent. **Owning the atomic
write is the only position from which the fusion is transactionally consistent under concurrent agents.**

That — not "liveness as a feature" — is why keel is a new VCS and not a layer. A layer can have all four
features and still be wrong under concurrency; keel cannot. This is the thing a git-wrapper, a review layer,
or a static index **structurally** cannot do.

### 1.4 The moat: defense-in-depth

The moat is **not** a single claim (early drafts over-rested on the data flywheel). It is three independent
layers, so there is no single point of failure:

1. **Transactional-fusion architecture** (§1.3) — copyable only by also being a VCS that owns the atomic write.
2. **Accumulated private feedback-weighted graph** — the context→verified-outcome mapping specific to a
   team's codebase evolution. Private, non-portable, and non-saturating because the code keeps changing.
3. **Switching cost** — once agents' working substrate + accumulated invariants live in keel, leaving means
   losing the graph.

Even if the data effect (layer 2) proves weaker than hoped, layers 1 and 3 hold.

### 1.5 The evidence (measured on prototypes; the bar the real build must hit)

| Claim | keel | naive baseline | harness |
|---|---|---|---|
| **Relevance context** (cross-file, 4 real repos) | 70–98% | grep/file-read 6–31% | `symbol-slice-ts.mjs` |
| — cross-language | Go 67%, Py 46% (heuristic) | 0–26% | `symbol-slice-lang.mjs` |
| **Write-path liveness** (uncommitted change) | 76% | static index 0% | `sourcegraph-freshness.mjs` |
| **Fused fetch** (behavior + is-it-safe, one call) | 59% | static index 0% | `coord-context-fetch.mjs` |
| **The flywheel** (prior session → next agent) | 75% ceiling / **72% realized** (graph+top-k) / 40% naive | no session 0% | `flywheel-lab.mjs`, `flywheel-graph-retrieval-lab.mjs` |
| **Coordination** (git conflict cost ÷ keel) | 1.5–9.7× cheaper | git merge-later | `coordination-lab.mjs` |
| **Storage engine** (LMDB, keel workload) | 1.5M reads/s, 7.6M conc, 0.64ms cold-open | redb/sled | `store-bench/` (Rust) |

Deterministic benches are byte-identical across runs (manifest SHA); LLM benches use LLM-judged grading with
Wilson CIs and reproduce within noise across independent runs.

### 1.6 Where keel sits

| player | what they are | keel's edge |
|---|---|---|
| **Sourcegraph** (Cody/Amp) | static, committed-state code graph | keel's graph is live (write-path) + carries sessions; 0% vs 76% on in-flight |
| **Entire / Dohmke** | "store session logs alongside code" (git-layer thesis) | keel **retrieves + fuses** sessions into context (flywheel); storage alone isn't the loop |
| **agentdiff / Agent Note** | git-layer provenance storage | native object + transactional fusion, not bolted-on git notes |
| **CodeRabbit / BugBot / Greptile** | AI review layers on GitHub | downstream of keel; potential integrators |
| **Model/agent platforms** (Claude memory, Cursor index, Copilot) | single-vendor, single-agent memory/context | keel is the **neutral cross-vendor multi-agent substrate**; coordination is structurally un-ownable by one vendor; their memory is a capture *input*, not competition |
| **git / GitHub / jj** | human-first VCS | agent-first; sessions, coordination, live graph, transactional fusion are native |

keel occupies the **intersection of Sourcegraph (code graph) and Entire (sessions)** — unoccupied, reachable
only off git. Git-layer players are structurally capped: sessions-as-metadata, committed-state, no native
coordination, **no transactional fusion**.

### 1.7 What we deliberately do NOT claim (honesty discipline)

- **Semantic-diff review compression** — real (~6× fewer tokens) but raises false-positives (21→35% at
  scale), is a wash vs savvy git flags, and incumbents ship it. **Commodity, not the story.** Do not lead with it.
- **"Compression improves detection"** — true only for specific weak-model/noise-bug pairs; NOT a universal law.
- The flywheel headline is **72% realized (graph retrieval), not 75%** — 75% is the oracle ceiling. The 72%
  used hand-modeled tags; a real extractor's recall is unproven (§7, NEW-1105).
- Everything is **prototype evidence in Node/TS**; the Rust product must reproduce it (§6).

---

## 2. Architecture & stack (all decided)

| Layer | Decision | Rationale / issue |
|---|---|---|
| **Core language** | **Rust** + language-resolver **sidecars** | perf/concurrency/startup; single static binary; precedent jj. NEW-1101 |
| **Resolvers** | TS-compiler + pyright in a Node sidecar (TS/Python **spine**); gopls for Go; tree-sitter for **breadth** | best resolver per language, don't rewrite. NEW-1067, NEW-1077/1078, NEW-1079 |
| **Storage engine** | **LMDB (`heed`)** primary, **redb** pure-Rust fallback; sled ruled out | benchmarked — won every axis; mmap B+tree + MVCC single-writer + COW no-WAL matches keel's read-heavy/concurrent/write-once/crash-only pattern. NEW-1074, `results-storage-engine.md` |
| **Object model** | content-addressed blob / tree / **change** / ref / **session**; **BLAKE3**; **FastCDC** chunk dedup; **jj change model**; git dropped as backing store | NEW-1074 |
| **Session object** | full artifact: task + **prompts (system+transcript)** + tool_calls[] + **tool_results[]** + reasoning_ref + model + economics + errors/retries + sub-sessions + verification | NEW-1086, NEW-1069 |
| **Identity / provenance** | Ed25519 **org→account→machine→session** delegation chain; every change+session signed | — |
| **Daemon** | long-lived, **crash-only**, one-per-machine + N workspaces | the live graph + coordination must stay warm; LMDB 0.64ms cold-open makes restart trivial |
| **Local IPC** | Unix domain socket (agent ↔ daemon) | — |
| **Remote transport** | **QUIC (`quinn`)** — multiplexed content-addressed chunk transfer + event channel, behind a pluggable **`SyncBackend`** interface; **HTTP/3-or-TCP fallback** for UDP-blocked networks | NEW-1102 |
| **Decentralization** | **nostr in v1 but DORMANT** — nostr `SyncBackend` + Ed25519↔schnorr bridge + ref→kind-31900 / blob→Blossom mapping; compiles, flagged **off**, seam-minimal | NEW-1103 |
| **git interop** | keel is the **standalone system of record**. git is an **optional edge adapter only**: import (one-way migrate-in) + optional export to a GitHub mirror for human reviewers during transition. **Never the backing store, never the authority.** "Export to PDF," not "built on PDF." | NEW-1097, NEW-1098 |
| **Intelligence** | deterministic graph traversal for relevance (no LLM in the hot path); LLM only for judgment (provider-agnostic routing) | — |

**One-liner:** a Rust-native, content-addressed store on LMDB holding code *and* full session artifacts, a
live graph over it, Ed25519 provenance, a jj change model, served through one `brief` fetch, synced over
QUIC — with a dormant nostr backend behind the same transport seam. keel is the system of record; git is an
optional export at the boundary.

---

## 3. Object & data model

Content-addressed, native (not a git layer):

| object | role |
|---|---|
| `blob` / `tree` | file content, like git |
| `change` | a diff — first-class, carries **intent + verification state** |
| `ref` | a named pointer |
| **`session`** | the agent session that produced a change (full schema, §2 / NEW-1086) — versioned like code; big blobs (transcripts, tool outputs) FastCDC-chunked |

**Every `change` references its `session`** → total causal provenance: every line traces to prompt + context
served + tool calls + model + green/red. The graph indexes code **and** sessions, so relevance retrieval
works over both. The fused write (§1.3) commits change + session + verification + reservation atomically.

**Privacy (required, not optional):** capture-time secret/PII scrubbing; **crypto-shredding** for erasure —
the immutable graph holds a hash+pointer, sensitive payloads live in an encrypted erasable side-store, so
deleting a key satisfies GDPR/CCPA right-to-erasure without breaking content-addressing (NEW-1088).

---

## 4. The primitives (each benchmark-gated)

- **Context / relevance service** (Epic NEW-1067) — import-aware symbol-slice: follow the real call/import
  graph, return the minimal task subgraph. Bar: 70–98%. Sidecar resolvers (§2). Workspace-aware for monorepos
  (30–35% of imports cross package boundaries). Task-scoped, budget-bounded (not blanket closure — hubs
  over-fetch).
- **The fused fetch `keel brief`** (Epic NEW-1068) — one call → context + coordination + provenance + sessions.
  Write-path live (working-tree state). Byte-stable, `--budget`-bounded, JSON-first. Bar: fused 59%, liveness 76%.
- **Session object + capture** (Epic NEW-1069) — first-class versioned session; ingest from Claude Code /
  Cursor / Aider (capture is commodity; the fusion is the value). Change→session linkage.
- **The flywheel** (Epic NEW-1070) — retrieve the relevant prior session for a new task (graph retrieval +
  top-k, not text similarity); log context→change→verified to improve the selector; **pin rule-lessons as
  pattern-attached invariants** (NEW-1100 — pays off from ONE prior session, not volume-gated). Bar: 0→72%.
- **Coordination** (Epic NEW-1071) — reservations **piggybacked on the brief** (free when uncontended);
  the coordinator predicts conflict before it happens. Bar: 1.5–9.7×, wins at every team size.
  *Shipped (single-daemon): reservations, import-graph-aware prediction (a held file that imports or is
  imported by the target is flagged, even across directories), a `keel reservations`/`keel release`
  visibility surface, durable holds across a daemon restart, and a reserve→land→free loop (a commit
  frees the holds on the files it changed). The cross-daemon ordered authority is the remaining piece
  and lives in the hosted layer.*
- **Live/incremental graph** (NEW-1075) — the hard, defensible core. Reuse existing incremental engines
  (stack-graphs / SCIP / tree-sitter); cache resolution, re-resolve only the changed subgraph; **latency SLO
  is a hard gate** (if the graph lags, "live" evaporates).

---

## 5. Build plan (milestones → issues)

Build the reference implementation of the validated primitives, then re-benchmark each real component against
its prototype bar. **Nothing ships below the bar.**

**M1 — Object model & live context graph.** NEW-1074 (native object model + LMDB), NEW-1075 (live graph),
NEW-1077/1078/1079 (TS/Python/breadth resolvers), NEW-1080 (monorepo), NEW-1081 (task-scoped subgraph),
NEW-1101 (Rust decision), NEW-1103 (nostr dormant seams), NEW-1107 (LMDB many-writers experiment).
*Gate: hold relevance 70–98% with real resolvers.*

**M2 — Sessions & the fused brief.** NEW-1086 (session schema), NEW-1087 (change→session), NEW-1088 (capture +
privacy), NEW-1089 (session CLI), NEW-1082 (`keel brief`), NEW-1083 (write-path liveness), NEW-1084 (token
contracts), NEW-1085 (verification state), NEW-1102 (QUIC).
*Gate: fused 59%, liveness 76% vs static-index 0%.*

**M3 — Flywheel & coordination.** NEW-1076 (retrieval-over-sessions), NEW-1090 (feedback edge), NEW-1091
(selector + e2e bench), NEW-1100 (pin invariants), NEW-1092 (reservations), NEW-1093 (conflict prediction),
NEW-1105 (real-extractor falsification — §7).
*Gate: flywheel realized 0→72% with real retrieval; coordination 1.5–9.7×.*

**M4 — Benchmark gate & adoption.** NEW-1094 (reproducible suite), NEW-1095 (CI gate), NEW-1096 (public
benchmark), NEW-1097 (git import), NEW-1098 (git export bridge), NEW-1099 (design-partner playbook), NEW-1104
(risk register), NEW-1106 (strong-competitor experiment — §7).
*Gate: one command reproduces every headline number; a design partner is live.*

**Critical path (the thin thread that must hold):**
`NEW-1074 (object model + LMDB) → NEW-1075 (live graph) + NEW-1077/1080 (resolvers) → NEW-1082/1083 (brief) →
NEW-1076/1092 (retrieval + coordination) → NEW-1094/1095 (gate).`
Start at 1074 + 1075 — nearly everything fetches through the brief, and the brief fetches through the live graph.

---

## 6. Non-negotiable engineering rules

1. **Rust for the core** (NEW-1101). Node/TS harnesses are the eval/spec layer only.
2. **Benchmark-gated.** Every real component must clear its prototype bar (§1.5) or the build is red (NEW-1095).
   Regression in any primitive = red.
3. **Deterministic where possible.** Byte-stable outputs (manifest SHA); fixed seeds/dates. LLM evals use
   Wilson CIs + LLM-judge grading; verify rates with adequate N before claiming (K=1/K=2 lie).
4. **Provenance always.** Every change + session signed under the delegation chain; the fused write is atomic.
5. **Privacy at capture.** Scrub secrets/PII; crypto-shred for erasure (NEW-1088). Never content-address a credential.
6. **Honesty.** Report what the numbers say, walk back overclaims. This is why the thesis is trusted.

---

## 7. Risks & the two experiments that gate the strategy

Full risk register: **NEW-1104**. The strategic cracks (adoption-vs-moat, platform disintermediation) are
dissolved by reframe (keel is the standalone system of record; neutral cross-vendor substrate — §1.6). The
remaining doubt is concentrated in **two must-run experiments**:

- **NEW-1105 — real-extractor flywheel falsification (highest value).** Replace the hand-modeled symbol tags
  with a REAL extractor over REAL repos + REAL captured sessions. **Bar: ≥60% end-to-end.** If it clears, the
  data moat is real. **If it can't clear the 40% naive floor, the honest pivot is: keel is a context/
  coordination LAYER, not a new VCS.** This experiment gates the strategy, not just a number.
- **NEW-1106 — beat a STRONG assembled competitor.** Build what an incumbent would (tree-sitter dep-graph +
  session store + coordination sidecar, integrated) and benchmark keel against *it*, not grep/0%. keel must
  win on the structural axes — especially **concurrency-consistency** (show the assembled layer produces an
  inconsistency that keel's atomic store prevents — the empirical form of §1.3).

Other tracked risks with mitigations baked into their issues: live-graph-at-scale (NEW-1075, SLO gate),
privacy/immutability (NEW-1088, crypto-shredding), QUIC/UDP (NEW-1102, TCP fallback), LMDB single-writer
(NEW-1107, per-repo sharding), jj/drop-git durability (LMDB ACID + fuzz gate + git-export escape hatch).

---

## 8. Reproducing the evidence

Harnesses live in `keel-bench/src/*.mjs` (Node) and `keel-bench/store-bench/` (Rust). Key runs:

- Relevance: `node src/symbol-slice-ts.mjs <repo>` · cross-lang `node src/symbol-slice-lang.mjs <py|go> <dir>`
- Liveness: `node src/sourcegraph-freshness.mjs` · Fused fetch: `node src/coord-context-fetch.mjs`
- Flywheel: `OPENROUTER_API_KEY=… node src/flywheel-graph-retrieval-lab.mjs --k 4`
- Coordination: `node src/coordination-lab.mjs` · Value proof: `node src/value-proof-deterministic.mjs`
- Storage: `cd store-bench && cargo run --release -- /tmp/sb-data`

Results writeups: `results-*.md` (storage-engine, flywheel-retrieval, workflow-lab, generalization, three-probes,
symbol-slice, semdiff-lab, value-proof, relevance-hardening, downstream). Overview: `keel-tech-sheet.md`.

---

## 9. Key numbers (memorize)

Relevance **70–98%** (grep 6–31%) · Liveness **76% vs 0%** · Fused **59% vs 0%** · Flywheel **0→72%** (ceiling
75, naive 40) · Coordination **1.5–9.7×** · Storage LMDB **1.5M reads/s, 0.64ms cold-open**. Model for LLM work
in the product: **claude-opus-4-8**. Two gating experiments: **NEW-1105** (flywheel real-extractor ≥60%),
**NEW-1106** (beat the assembled layer).

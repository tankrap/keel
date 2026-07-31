# Design: Explicit Checkpoints as Read Baselines

**Status:** Draft v1 — supersedes the "session cursor" mechanism in keel design v0 §4.
**Scope:** `keel` (CLI + daemon) read surfaces, output format rules, §10 accounting changes, and a benchmark gate for a deferred auto-cursor.
**Related:** keel design v0 (§4 output/token economy, §5 trust, §6 profiles, §9 identity, §10 metrics, §11 benchmarks); AI review platform design (plan-step diffs, claim ledger evidence).

---

## 1. Problem

Keel design v0 specified a **session cursor**: the daemon tracks "what the agent has been shown" and returns deltas against that baseline, with repeated identical looks costing ~1 token (`=`).

The cursor holds a daemon-side model of the agent's memory, and the agent can silently invalidate that model. Three failure modes, in increasing order of severity:

1. **Context compaction.** Harnesses compact, truncate, and summarize contexts continuously. The daemon believes the agent knows X; the harness compacted X away two turns ago; deltas now arrive against a baseline the model no longer has. This is the *common* case, not the failure case — v0's claim that cursor loss "degrades to full re-send, never wrongness" does not hold here, because the daemon doesn't know the loss occurred.

2. **The `=` response is pure reference.** "Nothing changed since your baseline" is only meaningful to a reader who remembers the baseline. To a compacted context it asserts "you already know the state" when the reader doesn't — silent wrongness in the scheme's cheapest and most frequent response. General lesson adopted as a design rule below: **omission is an encoding.** What a response leaves out is information the reader must reconstruct from memory; selection-statefulness and content-statefulness are not separable.

3. **"The agent's memory" is not a modelable object.** A session is often many LLM calls with contexts assembled fresh from a scratchpad; planner/executor sub-contexts share one workspace; subagents receive distilled notes. Continuity tokens get copied into notes and handoffs, so agent B echoes agent A's cursor and the daemon wrongly concludes B holds A's context. The premise — one continuous reader per workspace — does not survive contact with real harnesses.

Additionally, the economics that justified the cursor were computed in raw tokens. Under prompt caching, a **byte-identical re-sent digest** is often far cheaper in dollars than a **fresh unique delta** (cached input tokens are heavily discounted; every delta is novel bytes by construction). Deltas and prompt caching are structurally opposed economies, and v0 §10's counterfactual accounting — raw tokens saved vs a full dump — systematically overstates the delta's value.

## 2. Decision

1. **Ship explicit, named checkpoints as the only baseline primitive in v0.**
2. **Cut the inferred session cursor from v0 entirely.**
3. **Every response is self-grounding, including "nothing changed."**
4. **All read output is deterministic** — same query + same state pair = same bytes.
5. **An auto-advanced cursor may return post-v0 as sugar over checkpoints, gated on a keel-bench result in cache-adjusted dollars.**
6. **§10 accounting becomes cache-aware** before that gate can run.

Rationale, compressed: the daemon must never *guess* a baseline; baselines are *named*. Checkpoints are simple, correct under every harness behavior (including ones that don't exist yet), and independently useful to the review platform. The cursor is an optimization whose benefit is genuinely uncertain once caching enters the accounting — so it must prove itself on the bench, consistent with v0 §11 ("benchmarks gate everything").

## 3. The checkpoint primitive

### 3.1 Definition

A **checkpoint** is a cheap, named, content-addressed reference to a repo-visible state: the tuple of (commit graph frontier, working-copy commit, conflict set, ref positions) as seen by one workspace at one moment. It is a *read* concept — minting one mutates nothing and does not appear in the op log as a change.

- **Content-addressed:** the name is derived from the state (short handle over a BLAKE3 hash of the canonical tuple). Two workspaces at the same state derive the same name; a checkpoint name copied into a subagent's notes means exactly the same thing there. Names are globally meaningful; no per-session name tables.
- **Cheap:** minting is a hash over already-known heads — no snapshotting, no object writes beyond a small pin record.
- **Pinned, with GC by policy:** a minted checkpoint pins its frontier against pruning for a profile-configured TTL (default: generous, e.g. 30 days; `ci` preset shorter). Expired checkpoints fail closed (§7).

### 3.2 Minting is implicit and free

Every `st`, `save`, and `sync` response ends with a footer naming the checkpoint of the state it just described:

```
… ✓ main@c4f2 · ckpt k7
```

The agent never runs a "create checkpoint" command in the common path — it reuses a name it has already seen. An explicit `ckpt "label"` verb exists for deliberate naming (e.g., a harness marking plan-step boundaries), and accepts an optional human/agent label that aliases the content-addressed name.

### 3.3 Query surface

Every read verb accepts a baseline:

```
st @since:k7            what changed in my situation since k7
d  @since:k7 [path]     semantic diff of repo state since k7 (budgeted, drill-down as v0 §4)
log @since:k7           revset sugar: changes not visible at k7
```

Semantics are a pure function: **answer(query, from-state, to-state)** where to-state defaults to now. Deterministic, session-independent, testable, byte-stable for a given pair. `@since:` composes with existing revset queries; a checkpoint is usable anywhere a commit-ish is, plus it carries working-copy and conflict-set context that a bare commit doesn't.

### 3.4 Whose job is memory

Memory management belongs to the only party that knows what's in context: the caller. Harnesses keep checkpoint names in scratchpads exactly as they keep file paths. A caller that loses its names asks for a fresh full view — the same epistemic state as an agent that hasn't looked yet, served by the same commands. The daemon holds **no per-session read state** beyond credentials (§6).

## 4. Output rules (amendments to v0 §4)

### 4.1 Omission is an encoding

New stated design rule, sitting alongside v0's "absence is informative" as its precondition: absence is only informative when the response carrying it stands alone. Consequences:

- **No bare `=`.** The nothing-changed response is a micro-absolute fingerprint:

  ```
  = main@c4f2 · 2 files · 0 conflicts · ckpt k7
  ```

  ~15 tokens; stands alone in a context that has forgotten everything; still names the checkpoint so the chain continues.

- **Deltas are stateful in selection, stateless in content.** A baseline decides *which items* appear, never *how they are encoded*. Each item is the full current digest of that item keyed by stable change ID — never a relative patch against previously shown output ("+3 lines after the part you saw" is prohibited). A stale or forgotten baseline therefore yields exactly one failure mode: missing background, recoverable by expanding a handle. Wrongness is impossible by construction; this is v0's "digest by default, drill down by handle" applied temporally.

- **Every delta names its assumption.** Ten-token preamble: `Δ since k7 (st): 2 files`. An agent that doesn't recognize `k7` has a model-visible signal to re-baseline; remediation follows v0 error style (`unknown baseline? run: st`).

### 4.2 Determinism over adaptiveness

No adaptive widening, confidence decay, or heuristic output variation on any read surface. Same query + same state pair = same bytes, always. Preserves: byte-stability (v0 §4), prompt-cache warmth, benchmark reproducibility (§11), and debuggability ("why did the agent behave differently Tuesday" must have an answer). Any future variation in verbosity is profile-declared (v0 §6), never inferred.

### 4.3 Budget units

`--budget N` is defined in **bytes**, with a stated tokens-per-byte heuristic in docs (and per-model hints in profiles if needed). Token counts differ across model families; a contract needs stable units.

## 5. Cache-aware accounting (amendments to v0 §10)

Counterfactual accounting gains a pricing model:

- Per-response usage frames record novel bytes vs bytes eligible for provider prompt-cache reuse (byte-stable prefix match against prior responses in session).
- Headline metric becomes **cache-adjusted cost** (dollars, or cache-adjusted tokens: novel×1.0 + cached×discount), configurable per provider discount schedule; raw tokens remain reported.
- Replayed-task benchmarks (v0 §11) report both; the *task-level cache-adjusted cost* is the number that gates features and goes on the landing page. Per-call savings reward aggressive elision even when it causes extra round trips; task-level cost cannot be gamed that way.

## 6. Identity and sharing (interaction with v0 §9)

- Checkpoint **names** are content-addressed and freely shareable — meaningful across sessions, subagents, and machines by construction. This is what makes handoffs safe: no name means "what I showed *you*."
- Checkpoint **pins** (GC protection) attach to the minting session's credential chain; TTL and pin quotas are profile/org policy. Revoking a session does not invalidate names (they remain resolvable while any pin holds the frontier) but releases its pins.
- Any future per-session state (the deferred cursor, §8) binds to the session principal and is never resolvable from another session — the copied-token confusion in §1.3 becomes structurally impossible.

## 7. Failure modes

| Case | Behavior |
|---|---|
| Unknown/expired checkpoint in `@since:` | Fail closed with v0-style error: `!E_BASELINE k7 unknown → run: st (full view)` — never a silent full-send masquerading as a delta |
| Checkpoint from another workspace/machine | Resolvable if frontier present locally or fetchable; else same `!E_BASELINE` remediation |
| Frontier partially GC'd despite pin (corruption) | Verification failure per v0 §5; report, refuse to fabricate a diff |
| Caller compacted its names away | Not an error: caller requests fresh full view; response footer re-seeds the chain |
| Label collision on `ckpt "label"` | Labels are aliases scoped to (repo, org); collision returns existing mapping with its content name — labels never ambiguate, content names disambiguate |

## 8. Deferred: the auto-cursor (post-v0, bench-gated)

**What it would be:** sugar over checkpoints for the single-context case — the daemon auto-advances a per-session default baseline (the last checkpoint named in that session's responses) so bare `st` behaves as `st @since:<last>`. Implemented entirely on top of §3; deletable without breaking anyone.

**Correctness preconditions (from §4):** self-grounding responses, selection-only statefulness, named assumptions — all already mandatory, so the sugar inherits safety.

**The gate:** ships only if a replayed-task benchmark (v0 §11 corpus) shows auto-cursor beats **stable-full-digests-plus-prompt-caching** on cache-adjusted task cost (§5), with no regression in task success rate or round trips. Expected outcome: passes only for large working sets and long sessions — if so, enable per-profile (`agent` preset, monorepo scale), consistent with v0 §6.

**Optional harness sideband:** a `cursor reset` / context-epoch call for harnesses that know when they compact. Strictly an optimization (skips one redundant expansion); never a correctness requirement.

## 9. Review platform integration

Checkpoints are the evidence spine the review design wanted:

- **Plan-step diffs for free:** the CLI mints `ckpt "plan-step-N"` at each plan-step boundary (extends the provenance manifest's plan capture); the reconciliation engine's claim ledger cites `d @since:plan-step-2 @until:plan-step-3` as evidence regions. Claim → diff traceability becomes a pair of names, not a heuristic mapping.
- **Manifest anchoring:** the session's provenance manifest records the checkpoint chain (session-open, per-step, session-close), signed by the session principal per v0 §9 — resolving the review doc's open signing question and binding the two designs at one artifact.
- **Incremental re-review:** ledger-entry invalidation keys off checkpoint pairs; a new push mints a checkpoint and only claims whose evidence interval changed are re-verified.

## 10. What is explicitly cut from v0

- Daemon-side "what the agent has been shown" tracking, in any form.
- Bare `=` responses and all relative-to-previous-output encodings.
- Confidence decay / adaptive output widening.
- Raw-token counterfactual as the headline savings metric.

## 11. Open questions

1. Canonical tuple for the checkpoint hash — exact fields and normalization (must be stable across daemon versions; version the tuple schema).
2. Pin quota policy per preset; behavior when an org exceeds pin storage (LRU release vs hard fail).
3. `@until:` on all verbs (state-pair queries) in v0, or `@since:`-only with `@until:` following the review-platform need — leaning v0, cost is small once the pure-function core exists.
4. Provider cache-discount schedules in §5: static config vs measured; multi-provider sessions (OpenRouter) complicate attribution.
5. Whether `log @since:` needs its own pagination checkpoint for very large intervals, or budget+elision handles (v0 §4) suffice.

## 12. Build order

1. Checkpoint core: canonical tuple, hashing, pin records, footer minting on `st`/`save`/`sync`.
2. `@since:` on `st` and `d`; self-grounding `=`; delta preambles; `!E_BASELINE`.
3. `ckpt "label"` verb + manifest chain recording (unblocks review-platform plan-step evidence).
4. Cache-aware usage frames + accounting change (§5) — prerequisite for any cursor work.
5. `log @since:`, `@until:`, cross-machine resolution.
6. (Post-v0, gated) auto-cursor sugar + harness sideband, behind the §8 benchmark.

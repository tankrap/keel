# keel: semantic-diff depth + the decentralized substrate

Design note, 2026-07-27. Two threads: how deep the semantic diff should go (and
which change-types each depth helps), and how a nostr substrate fits without
dissolving the moat. Linear: project *keel: decentralized substrate +
semantic-diff depth* (NEW-1011…1022).

## 1. The semantic-diff depth ladder

Today's `keel review` is **lexical**, not structural: mask each changed line to a
shape (identifiers→`_`, digits→`#`, whitespace collapsed, operators kept), group
hunks by shape, a shape recurring ≥3× is "mechanical" (collapsed), unique hunks
are "substantive". Plus git's `-M` rename detection and one regex for
function-definition symbols. That's ~60 lines — powerful, but shallow, and
**replicable by anyone**. Depth is the moat gradient.

| level | mechanism | best for | fixes / catches |
|---|---|---|---|
| **0 — shipped** | lexical mask + frequency grouping | pure mechanical bulk (renames, scale edits, formatting); operator-shape anomalies (sign flip) | — |
| **1 — prototyped ✓** | literal-anomaly split + representative instance + richer symbols | mixed mechanical+substantive PRs; codemods with a smuggled constant | hidden-constant bugs (numbers were masked away); benign over-flagging; class/method/arrow/const symbols |
| **2** | AST (tree-sitter) + scope/binding | safe consistent rename vs variable-capture; accurate symbol/hunk boundaries; multi-language | lexical false anomalies; per-symbol diffs |
| **3** | dataflow / type-aware | logic bugs where *what flows where* changed: tax-on-wrong-base, swapped args, inverted conditions, bounds off-by-one | the "substantive but subtle" class, structurally |
| **4** | cross-file symbol + dependency graph, fleet-memoized | ripple/impact ("sig change breaks N callers"), stale-cache/invalidation, cross-module TOCTOU | whole-repo context |

**Key insight — each level buys a different change-type, and depth tracks
defensibility.** Level 0 is a commodity (copy the masking). Levels 3–4 need a
persistent, content-addressed, memoized graph — too expensive to recompute
per-node — so they can only live in a shared hosted service. *The deeper the
semantic diff, the more defensible it is.*

### Level 1 is proven (deterministic, no API)

`keel-bench/src/semantic-v2.mjs` implements two upgrades and measures them on the
exact change-types Level 0 gets wrong:

- **Literal-anomaly split.** Within a mechanical group, keep each site's concrete
  literal-vector; surface a minority vector as an anomaly. On
  `outlier-numeric-in-rename` (a `input*2`→`input*3` smuggled into a 15-file
  rename), the bug goes **HIDDEN → SURFACED** — while a uniformly-varying group
  (`i=0..7` across 20 files) stays fully compressed (no false anomaly).
- **Representative instance.** Show one concrete example per mechanical group, so
  a reviewer stops over-flagging benign compressed blocks (the false-positive
  mode we measured). Compression is preserved.

These two fixes target precisely the failures the review evals surfaced (the
masked-constant miss and the mechanical-block false positive). Next: port into
the Rust `cmd_review` (NEW-1017), then climb to AST/dataflow (NEW-1018…1020).

## 2. Git interoperability (adoption path)

Agents have no git muscle memory — that's our wedge — but repos and humans do.
Interop lowers the barrier while accumulated state creates the lock-in:

- `keel import <git-repo>` — ingest history/refs/blobs into the content-addressed
  store (dedup for free).
- A `git` remote helper so existing tooling/CI keeps working during migration.
- "Runs alongside GitHub" mode: mirror refs, add keel review/CI/coordination on
  top without a forced cutover.
- Map git authors → keel identities where possible; mark pre-keel history as
  unattested (honest provenance).

## 3. The nostr decentralized substrate

Reconciling decentralization with the moat by **splitting the layers**:

- **Substrate on nostr (open, portable):** identity, signed provenance, and
  ref/metadata transport as signed events on relays. Strengthens the provenance +
  distribution pillars and removes the "your history is hostage" objection — which
  we can give away *because the real lock-in is the intelligence, not the data*.
- **Intelligence hosted (the moat):** deep semantic diff (L3–4), memoized CI,
  fleet coordination, the verification graph. Relays are dumb stores; the smart
  layer indexes the decentralized event stream and adds the expensive, stateful
  smarts. Users *can* self-host transport and never be locked in — and come to us
  for the intelligence anyway, because it's too costly to recompute per-node.

### Working prototype — `keel/src/nostr/`

`keel-nostr.mjs` + `demo.mjs` (roundtrip passes, no network):

- **Real NIP-01 events**, schnorr-signed (nostr-tools), correct event ids.
- **Refs** as parameterized-replaceable events (kind 31900, latest-wins per
  `repo#ref`); the relay collapses replacements to one.
- **The Ed25519 ↔ secp256k1 bridge.** nostr signs with secp256k1/schnorr; keel's
  authority is Ed25519. Solution: a nostr event (schnorr-signed for relay
  compatibility) whose `content` carries the keel Ed25519-signed provenance
  bundle. A verifier checks **both** — the schnorr sig (transport authenticity)
  and the keel chain (delegated authority). The demo verifies both and **rejects
  a tampered provenance claim** via the Ed25519 chain.

### Honest costs of going decentralized

- **Blobs don't fit nostr events** (small JSON) — packs/blobs go to
  content-addressed blob storage (blossom-style), referenced by hash.
- **Relays are eventually-consistent / last-writer-wins** — fine for publishing
  history, but it's exactly why **coordination stays on the ordered hosted
  authority, not nostr** (reservations need consistency; the substrate can't give
  it).
- Smaller ecosystem + protocol risk; prior nostr-git efforts (NIP-34, ngit)
  prove feasibility but not traction.

## 4. Is what we have defensible today? (straight answer)

**Not yet a strong moat as-is — but the direction closes the gap, and two pieces
are already differentiated.**

- **Commodity today:** the AI review *pipeline* (tiered routing, multi-model,
  semantic diff, confidence triage) — all exist and are funded (CodeRabbit $60M @
  $550M, BugBot, Greptile, Cubic). Level-0 semantic diff is copyable.
- **Already differentiated (defensible-ish):** (1) the measured finding that
  **compression improves *detection*, not just cost** (cheap model on semantic
  diff caught a retry off-by-one 100% vs 10% for a strong model on the full
  diff) — nobody else claims this; (2) **identity/provenance as a VCS primitive**
  (the Ed25519 chain + now the nostr bridge) — incumbents are GitHub layers and
  structurally can't own this.
- **The actual moat is still to be built:** the **accumulated agent-context
  graph** (verification history, provenance, dependency graph, coordination) that
  exists only because work runs on-platform and compounds with usage, plus the
  **fleet network effects** (memoized CI, shared caches). Depth Levels 3–4 and the
  hosted intelligence layer (NEW-1014) are what turn "nice features" into
  "cheaper-and-smarter the more you use it, and it doesn't transfer off."

**Verdict:** today = a strong, well-measured *product wedge* with two real
differentiators, but a *replicable* one until the state layer accumulates. The
plan (deepen the diff past commodity, own identity/provenance on an open
substrate, accumulate the context graph centrally) is the right shape to convert
the wedge into a moat. Ship the wedge to get fleets in the door; let the state
compound to keep them.

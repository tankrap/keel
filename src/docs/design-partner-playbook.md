# keel — design-partner playbook

*Grounded in what's built + measured (see the benchmark report). Strategy calls marked **[founder]** are for the founder to confirm, not settled by engineering.*

## The one-line wedge
Give an AI agent fleet a **live fused brief** — the exact task-relevant code, its dependency graph, provenance, verification, coordination, and the relevant prior session — in one fetch. No incumbent does this: git has no context engine, Sourcegraph is a committed-state index (0% on in-flight edits), Entire captures sessions but doesn't retrieve them as live context.

## Ideal design partner (ICP)
Not "a team we convince to leave GitHub." The partner is:
- **Running a heterogeneous multi-agent fleet on shared code** (Claude Code + Cursor + CI agents, several at once), and
- **Already in pain** from it: agents clobbering each other, re-deriving the same project rules, no provenance for "why did the agent write this."
- Mid-size codebase (not so small the fleet is 1 agent; not a 10-year monolith where a switch is heavy).

Where the switch pays for itself in a week, not a quarter.

## Why they don't have to switch anything (the on-ramp)
- `keel import <git-repo>` ingests their repo as native history in one shot (verified on the 94.5k-file linux kernel, clean).
- Agents get the brief via `keel brief` / the warm daemon **alongside** their existing workflow — no cutover.
- `keel export` mirrors keel history back to a GitHub repo so human reviewers stay on GitHub during transition.
- keel is the system of record; git is an optional, removable edge adapter. Data flows keel→git, never git→keel-as-authority.

## The pilot (2–4 weeks)
1. **Import** their repo; stand up the daemon (warm graph + coordination).
2. **Capture** a week of real agent sessions (`keel commit --session`); pin 3–5 known project invariants (`keel pin`).
3. **Wire** one agent harness to fetch `keel brief` before acting, and to record `context_served` + verify green/red.
4. **Measure** against their own baseline (below).

## What to measure (their numbers, not ours)
- **Correctness lift**: task success WITH vs WITHOUT the brief, on *their* recurring project rules. (Our controlled eval: 70%→100%, +30pts, concentrated on non-obvious rules.)
- **Coordination**: merge-conflict / clobber incidents/week before vs after reservations. (Model predicts 1.5–9.7× fewer.)
- **Token economy**: tokens/task — the brief serves a ~300-token median slice vs feeding files/greps.
- **Provenance**: can they answer "why was this line written" (session → prompt → verdict) — a yes/no they can't get today.

Success bar **[founder]**: pick one metric that must move (recommend correctness lift on their rules, or clobber-rate) and a threshold before the pilot starts.

## Honest limits to say up front
- Storage is **larger than git** (~4× on the kernel) — keel trades bytes for context; don't pitch it as cheaper storage.
- Multi-agent coordination requires the **shared daemon** (it's live there; not across independent CLI processes).
- The flywheel lift is proven in a controlled eval on authored rules; the pilot is partly *to get the real-corpus number*.
- Transports are local today (Unix socket); multi-machine (QUIC) is designed, not built — fine for a single-host fleet pilot.

## Open-core / pricing **[founder]**
- **OSS (permissive):** the single-node substrate — store, resolvers, live graph, brief, CLI, local coordination. Drives adoption + neutrality (cross-vendor).
- **Paid (hosted):** multi-tenant coordination authority, the org-private flywheel graph (the durable moat + switching cost), retention/compliance (capture-time scrub + crypto-shredding), SSO/audit. Priced per-fleet/seat, **not** per-GB.
- Sequence: land the OSS wedge → the accruing org flywheel graph is what converts to paid.

## Risks to watch in a pilot
- Cold start: mitigated by import (seeds history) + pinned invariants (pay off from one pin, no volume needed).
- Capture privacy: transcripts/tool-outputs can hold secrets — scrub + crypto-shred before this touches a real repo (NEW-1088, required before a partner's private code).
- "Is it real?": the benchmark report + the pilot's own numbers are the answer; lead with their metrics.

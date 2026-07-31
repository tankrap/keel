# Flywheel retrieval-lift — live result

`flywheel_bench.py`, Rust keel. Solver = `claude-opus-4-8` (the agent under test), dual judge =
`claude-sonnet-5`. **n = 20 scenarios × 3 trials = 60 samples per condition** (parallel, backoff on
rate limits).

Each scenario carries an **arbitrary project convention** a competent model can't guess a priori
(e.g. "all logging goes through `audit(event, {traceId})`, never `console.log`"; "timestamps are
integer `utcMillis()`"; "handlers call `assertTenant(ctx)` first"; "money is integer Cents,
formatted with `fmtMoney`"; "gate features with `flag('name', ctx)`"). Each convention is recorded
in a real keel repo via `keel learn`; `keel brief` retrieves it for the task. We solve the task
WITHOUT the brief and WITH it, 3 trials each, and a strict dual judge scores rule-compliance.

| condition | correct | 95% CI (Wilson) | |
|---|---|---|---|
| WITHOUT keel brief | **0 / 60 (0%)** | 0–6% | the model never invents the arbitrary convention |
| WITH keel brief    | **57 / 60 (95%)** | 86–98% | the retrieved lesson makes it comply |
| **lift** | **+95 points** | CIs disjoint | |

All **20/20** lessons were retrieved (`keel brief` surfaced the right prior lesson every time).
Every scenario passed 3/3 WITH the brief except **price.js** (0/3), where the model won't fully
comply with the integer-Cents / `fmtMoney` rule even when handed the lesson — an honest ceiling,
not a retrieval miss (the same scenario missed in the earlier n=10 run).

**Reading:** the lift concentrates exactly where the thesis predicts — *non-obvious, project-local
rules*, unknowable a priori, that a fused brief carries from prior work. On obvious tasks both
conditions would pass and there'd be no lift; the value is the arbitrary knowledge keel accumulates
and retrieves. The two conditions' 95% CIs don't overlap (0–6% vs 86–98%), so the 0%→95% signal is
not a run-to-run artifact — it reproduces across 60 controlled samples. Temperature is model-default
(adaptive thinking); the 3-trial-per-scenario design absorbs that variance directly.

_Earlier controlled run for reference: n=10, 1 trial each → WITHOUT 0/10, WITH 8/10, +80 points._

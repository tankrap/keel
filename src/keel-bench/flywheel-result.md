# Flywheel retrieval-lift — live result

`flywheel_bench.py`, Rust keel, solver + dual-judge = claude-opus-4-8, n=10, 1 trial each.

Each scenario carries an **arbitrary project convention** a competent model can't guess a priori
(e.g. "all logging goes through `audit(event, {traceId})`, never `console.log`"; "timestamps are
integer `utcMillis()`"; "handlers call `assertTenant(ctx)` first"). Each convention is recorded in
a real keel repo via `keel learn`; `keel brief` retrieves it for the task. We solve WITHOUT the
brief and WITH it, and a strict dual judge scores rule-compliance.

| condition | correct | |
|---|---|---|
| WITHOUT keel brief | **0 / 10 (0%)** | the model never invents the arbitrary convention |
| WITH keel brief    | **8 / 10 (80%)** | the retrieved lesson makes it comply |
| **lift** | **+80 points** | |

All 10 lessons were correctly retrieved (`keel brief` surfaced the right prior lesson every time).
The 2 WITH misses (price, errors) are honest — even given the lesson the model didn't fully comply.

**Reading:** the lift concentrates exactly where the thesis predicts — *non-obvious, project-local
rules*, unknowable a priori, that a fused brief carries from prior work. On obvious tasks both
conditions would pass and there'd be no lift; the value is the arbitrary knowledge keel accumulates
and retrieves. This is a single controlled run (n=10); temperature is model-default (adaptive
thinking), so WITH may vary by a scenario or two run-to-run — the 0%→80% signal is the point.

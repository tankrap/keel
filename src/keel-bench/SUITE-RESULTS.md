# keel flywheel benchmark suite — results

Live LLM benchmarks: does surfacing the non-obvious prior lesson for a task make an agent measurably
more correct? Solver = `claude-opus-4-8` (the agent under test), strict dual judge = `claude-sonnet-5`.
**4 trials per scenario per condition**, parallel with backoff, Wilson 95% CIs. `keel learn` records a
convention → `keel brief` retrieves it → solve WITHOUT the brief vs WITH it.

## Correctness lift — across three languages

| Benchmark | n/cond | WITHOUT | WITH | lift |
|---|---|---|---|---|
| **Synthetic** (invented, non-guessable rules) | 80 | **0%** (CI 0–5) | **95%** (CI 88–98) | **+95** |
| **Real — VS Code** (TypeScript) | 64 | **73%** (62–83) | **94%** (85–98) | **+20** |
| **Real — Django** (Python) | 48 | **62%** (48–75) | **92%** (80–97) | **+29** |
| **Real — Prometheus** (Go) | 48 | **58%** (44–71) | **90%** (78–96) | **+31** |
| **Pooled real corpus (3 languages)** | 160 | **66%** (58–73) | **92%** (87–95) | **+26** |

Every convention retrieved (60/60 across the real corpora; 20/20 synthetic).

## Adversarial control — the lift is the *specific* rule, not "more context"

Skeptic's hypothesis: maybe WITH only wins because it appends *any* authoritative-looking "convention
from this codebase's history." Tested with a third condition on the synthetic scenarios — **decoy**: a
different scenario's real rule (plausible, authoritative-looking, but *wrong* for the task).

| condition | 80/cond | |
|---|---|---|
| WITHOUT (no lesson) | **0%** | baseline |
| WITH (**correct** rule) | **95%** | **+95** |
| DECOY (a **wrong** rule) | **0%** | **+0** |

A wrong retrieved rule gives **zero** lift — identical to no lesson. The gain is entirely the specific
retrieved content; the prompting-artifact hypothesis is rejected.

## Reading

- **Synthetic (0→95)** isolates the mechanism at maximum headroom: on rules a model provably can't
  guess (arbitrary conventions), retrieval goes from *never right* to *almost always right*; the CIs
  are disjoint.
- **Real corpora generalize across TypeScript, Python, and Go.** Conventions were mined from
  `microsoft/vscode`, `django/django`, `prometheus/prometheus` and **grounded by their frequency in
  the actual source** — nothing invented. A telling pattern: as the model's prior familiarity with a
  language's conventions falls, the baseline falls (TS 73 > PY 62 > Go 58) and the **lift rises**
  (+20 → +29 → +31). The lift always concentrates on the conventions the model had *not* memorized
  (Django `gettext_lazy` 0→4/4, `get_object_or_404` 1→4; Go `fmt.Errorf %w` 0→4/4, `ctx`-first 0→4;
  VS Code `URI.joinPath`, `assertNever`).
- **Honest ceilings** (unchanged): a few conventions don't move even given the lesson — Go
  `storage/remote/client.go` body-close (0→0), Django `timezone.now` (0→0), VS Code `coalesce`. Model
  ceilings, not retrieval misses (all retrieved).

**Bottom line:** the flywheel is genuine, specific knowledge transfer on the subset of project-local
rules not already in the weights — reproducing across invented rules and three real languages, and it
survives an adversarial control that a generic-prompting effect would have failed.

## Reproduce

```bash
cd src/keel-bench
python3 run_suite.py --dry-run                        # validate harnesses, no API (free)
TRIALS=4 WORKERS=8 python3 flywheel_bench.py           # synthetic
TRIALS=4 WORKERS=8 python3 corpus_bench.py             # VS Code    (~/keel-vscode-demo)
TRIALS=4 WORKERS=8 python3 corpus_bench_django.py      # Django     (~/keel-django-demo)
TRIALS=4 WORKERS=8 python3 corpus_bench_go.py          # Prometheus (~/keel-go-demo)
TRIALS=4 WORKERS=8 python3 flywheel_adversarial.py     # 3-condition adversarial control
```

Key from `~/.claude-token` (never logged); no `temperature` (opus-4-8 adaptive thinking). Corpora are
blobless shallow clones; per-file keel repos keep retrieval clean.

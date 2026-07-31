# keel flywheel benchmark suite — results

Live LLM benchmarks of the flywheel: does surfacing the non-obvious prior lesson for a task make an
agent measurably more correct? Solver = `claude-opus-4-8` (the agent under test), strict dual judge =
`claude-sonnet-5`. **4 trials per scenario per condition**, parallel with backoff, Wilson 95% CIs.
`keel learn` records a convention → `keel brief` retrieves it → solve WITHOUT the brief vs WITH it.

| Benchmark | scenarios × trials | WITHOUT | WITH | lift | retrieved |
|---|---|---|---|---|---|
| **Synthetic** (invented, non-guessable rules) | 20 × 4 = 80/cond | **0%** (CI 0–5) | **95%** (CI 88–98) | **+95** | 20/20 |
| **Real corpus — VS Code** (TypeScript) | 16 × 4 = 64/cond | **73%** (CI 62–83) | **94%** (CI 85–98) | **+20** | 16/16 |
| **Real corpus — Django** (Python) | 12 × 4 = 48/cond | **62%** (CI 48–75) | **92%** (CI 80–97) | **+29** | 12/12 |
| **Pooled real corpus** (2 languages) | 112/cond | **69%** (CI 60–77) | **93%** (CI 86–96) | **+24** | 28/28 |

Every convention retrieved (48/48 total). All numbers reproduce the earlier 3-trial runs with tighter
intervals.

## Reading

- **Synthetic (0→95)** isolates the mechanism at maximum headroom: on rules the model provably can't
  guess a priori (arbitrary project conventions), retrieval takes it from *never right* to *almost
  always right*. The two CIs (0–5% vs 88–98%) are disjoint — not a run-to-run artifact.

- **Real corpora generalize across two languages.** On real, documented conventions mined from
  `microsoft/vscode` (TS) and `django/django` (Python) — each grounded by its frequency in the actual
  source, nothing invented — the lift holds: **+20 (VS Code), +29 (Django), +24 pooled.** This is the
  hard test: Opus was *trained on both codebases*, so it already knows the famous idioms; the baseline
  is high (69% pooled) and there's little headroom. The lift concentrates exactly on the conventions
  it had **not** memorized — Django i18n `gettext_lazy` (0/4→4/4), `get_object_or_404` (1→4),
  `settings` access (1→4); VS Code `URI.joinPath`, `assertNever`, `IConfigurationService`.

- **Honest ceilings.** A few conventions don't move even given the lesson — Django `timezone.now`
  (0/4→0/4), VS Code `coalesce`, the synthetic `price.js` integer-Cents rule. The model won't comply
  with those regardless; that's a model ceiling, not a retrieval miss (all were retrieved).

**Bottom line:** the flywheel's value is genuine knowledge transfer on the subset of project-local
rules not already in the weights — and it reproduces across invented rules, TypeScript, and Python.
Retrieval pays even against a model that memorized the target codebases.

## Reproduce

```bash
cd src/keel-bench
python3 run_suite.py --dry-run          # validate harnesses, no API calls (free)
TRIALS=4 WORKERS=8 python3 flywheel_bench.py          # synthetic
TRIALS=4 WORKERS=8 python3 corpus_bench.py            # VS Code   (CORPUS_SRC=~/keel-vscode-demo)
TRIALS=4 WORKERS=8 python3 corpus_bench_django.py     # Django    (CORPUS_SRC=~/keel-django-demo)
```

Key from `~/.claude-token` (never logged); no `temperature` (opus-4-8 adaptive thinking). Corpora are
blobless shallow clones of the two repos; per-file keel repos keep retrieval clean (brief also
surfaces graph-neighbour lessons, which would cross-contaminate in a shared repo).

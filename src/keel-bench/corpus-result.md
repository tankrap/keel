# Real-corpus flywheel lift — VS Code

`corpus_bench.py`, Rust keel. Solver = `claude-opus-4-8` (agent under test), dual judge =
`claude-sonnet-5`. **16 real VS Code conventions × 3 trials = 48 samples per condition.**

This run answers the obvious objection to the synthetic benchmark — *"you invented those
conventions, of course retrieval helps."* Every convention here is a **real, documented VS Code
rule**, grounded by its frequency in a fresh `microsoft/vscode` checkout (17k files):

| convention | evidence in the real source |
|---|---|
| `undefined`, never `null` | 66,458 `undefined` vs **0** `: null` type usages |
| `this._register(...)` disposables | 12,787 |
| `@IXxxService` DI injection | 15,401 |
| `Emitter` / `onDidX` events | 3,517 / 3,806 |
| `URI` not path strings | 12,242 |
| `CancellationToken` | 6,265 |
| `localize(...)` for UI strings | 21,381 |
| `registerSingleton` | 677 |
| `assertNever` exhaustiveness | 44 |

Each convention was attached (via `keel learn`) to the **real VS Code file** it governs — real path,
real content — and retrieved by `keel brief`. Solve the task WITHOUT the brief and WITH it, 3 trials
each; a strict dual judge scores compliance with the real rule.

| condition | correct | 95% CI (Wilson) |
|---|---|---|
| WITHOUT keel brief | **36 / 48 (75%)** | 61–85% |
| WITH keel brief    | **45 / 48 (94%)** | 83–98% |
| **lift** | **+19 points** | — |

All **16/16** conventions were retrieved.

## Where the lift lands — and why that's the real result

The baseline is **high (75%)** for a telling reason: Opus was trained on VS Code, so it already
*knows* the codebase's most famous public idioms. On 11 of 16 conventions it complied **3/3 without
any brief at all** — Disposable/`_register`, `Emitter` events, `CancellationToken`, DI decorators,
`registerSingleton`, `localize`, `undefined`-not-null, the async helpers. There is no headroom to
lift a rule the model has already memorized, and retrieval correctly neither helped nor hurt them.

The lift concentrates **exactly on the conventions the model had NOT internalized**:

| convention | WITHOUT | WITH |
|---|---|---|
| `URI.joinPath` for child paths (not string concat) | **0/3** | **3/3** |
| `assertNever` to close an exhaustive switch | **0/3** | **3/3** |
| `IConfigurationService.getValue(...)` | 1/3 | 3/3 |
| `isEqual(a,b)` for resource comparison | 2/3 | 3/3 |

One honest ceiling: `coalesce(...)` for dropping nullish entries stayed **0/3 → 0/3** — even handed
the rule, the model prefers `filter(Boolean)`.

**Reading.** This is a harder, more credible test than the synthetic run. Against a model that has
*memorised the target codebase's public conventions*, a fused brief still lifts correctness by 19
points — and every point comes from a real rule the model didn't already know (URI-join semantics,
exhaustiveness discipline, the configuration service). That's genuine knowledge transfer, not the
model parroting training data. The paired improvement is significant (4 conventions improved, 0
regressed; at the trial level 9 discordant pairs, all in keel's favour, McNemar p ≈ 0.004).

The two runs bracket the effect: the synthetic run (novel conventions, 0%→95%) isolates the
mechanism at maximum headroom; this real-corpus run (VS Code, 75%→94%) shows it still pays on a real
codebase with a knowledgeable model — on precisely the subset of knowledge that isn't already in the
weights, which is the only subset a retrieval system can ever help with.

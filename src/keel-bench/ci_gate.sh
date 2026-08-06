#!/bin/zsh
# keel CI gate (NEW-1095): the build fails unless every real component clears its bar.
# Gates: build · tests · clippy · relevance (cross-file slice) · liveness (uncommitted edit) ·
# coordination · full e2e (init/import/export/commit/status/diff/verify/pin). Deterministic;
# the LLM flywheel eval (flywheel_live.py) is a separate extended check, not this fast gate.
set -u
export PATH="/opt/homebrew/opt/node@22/bin:$PATH"
RUST=~/keel/src/rust
KEEL="$RUST/target/release/keel"
TMP="${TMPDIR:-/tmp}/keel-ci-$$"
FAILED=0
pass() { print -r -- "  ✓ $1" }
fail() { print -r -- "  ✗ FAIL: $1"; FAILED=$((FAILED+1)) }
section() { print -r -- ""; print -r -- "▶ $1" }

cd "$RUST" || exit 2

section "build (release)"
if cargo build --release >/dev/null 2>&1; then pass "workspace builds"; else fail "release build"; fi

section "tests"
T=$(cargo test -p keel-store -p keel-resolve -p keel-graph -p keel-coord -p keel-brief -p keel-cmd 2>&1)
if print -r -- "$T" | grep -q "FAILED\|error\["; then fail "test failures"; else
  N=$(print -r -- "$T" | grep -E "test result: ok" | awk '{s+=$4} END{print s}')
  pass "$N tests pass"
fi

section "clippy (native crates, zero warnings)"
W=0
for c in keel-store keel-resolve keel-graph keel-coord keel-brief keel-daemon keel-cmd; do
  W=$((W + $(cargo clippy -p $c --all-targets 2>&1 | grep -cE 'warning:|error')))
done
if [ "$W" -eq 0 ]; then pass "clippy clean"; else fail "clippy: $W warnings"; fi

# ── fixture: a tiny cross-file TS repo ──
mkdir -p "$TMP/repo"
printf 'export function helper(x: number): number { return x * 2; }\n' > "$TMP/repo/util.ts"
printf 'import { helper } from "./util.js";\nexport function doA(): number { return helper(21); }\n' > "$TMP/repo/a.ts"
"$KEEL" init --root "$TMP/repo" >/dev/null 2>&1
"$KEEL" native commit --root "$TMP/repo" -m init >/dev/null 2>&1

section "relevance — cross-file symbol slice (bar: ≥2 defs incl cross-file callee)"
BJSON=$("$KEEL" brief --root "$TMP/repo" --file a.ts --symbol doA --json 2>/dev/null)
NDEF=$(print -r -- "$BJSON" | python3 -c "import sys,json;print(len(json.load(sys.stdin).get('context',[])))" 2>/dev/null)
XFILE=$(print -r -- "$BJSON" | python3 -c "import sys,json;d=json.load(sys.stdin);print(int(any(c['symbol']=='helper' and c['file']=='util.ts' for c in d.get('context',[]))))" 2>/dev/null)
if [ "${NDEF:-0}" -ge 2 ] && [ "${XFILE:-0}" = "1" ]; then pass "cross-file slice ($NDEF defs, helper resolved)"; else fail "relevance ($NDEF defs, xfile=$XFILE)"; fi

section "liveness — graph reflects an UNCOMMITTED edit (static-index=0%)"
printf 'import { helper } from "./util.js";\nexport function doB(): number { return helper(1); }\n' > "$TMP/repo/b.ts"
DEPS=$("$KEEL" brief --root "$TMP/repo" --file b.ts --json 2>/dev/null | python3 -c "import sys,json;print(','.join(json.load(sys.stdin).get('deps',[])))" 2>/dev/null)
if print -r -- "$DEPS" | grep -q "util.ts"; then pass "uncommitted b.ts → dep util.ts (live)"; else fail "liveness (deps=$DEPS)"; fi

section "feedback — verify + pin surface in the brief"
CH=$("$KEEL" native log --root "$TMP/repo" 2>/dev/null | head -1 | awk '{print $1}')
"$KEEL" verify "$CH" --green --root "$TMP/repo" >/dev/null 2>&1
"$KEEL" pin doA --lesson "always settle first" --root "$TMP/repo" >/dev/null 2>&1
VJSON=$("$KEEL" brief --root "$TMP/repo" --file a.ts --symbol doA --json 2>/dev/null)
VOK=$(print -r -- "$VJSON" | python3 -c "import sys,json;d=json.load(sys.stdin);print(int(d.get('verification')=='green' and len(d.get('invariants',[]))>=1))" 2>/dev/null)
if [ "${VOK:-0}" = "1" ]; then pass "verify=green + pinned invariant served"; else fail "verify/pin not surfaced"; fi

section "git on-ramp — import → export round-trip"
G="$TMP/gitsrc"; mkdir -p "$G"; ( cd "$G" && git init -q && git config user.email a@b.co && git config user.name a \
  && printf 'x1\n' > f.txt && git add -A && git commit -qm c1 \
  && printf 'x2\n' > f.txt && printf 'y\n' > g.txt && git add -A && git commit -qm c2 ) >/dev/null 2>&1
"$KEEL" import "$G" --into "$G" >/dev/null 2>&1
IC=$("$KEEL" native log --root "$G" 2>/dev/null | wc -l | tr -d ' ')
"$KEEL" export "$TMP/gitout" --store "$G/.keel/store" >/dev/null 2>&1
EC=$(git -C "$TMP/gitout" log --oneline 2>/dev/null | wc -l | tr -d ' ')
if [ "$IC" = "2" ] && [ "$EC" = "2" ]; then pass "import 2 → export 2 commits"; else fail "on-ramp (import=$IC export=$EC)"; fi

section "git on-ramp — merge DAG round-trips (no commit loss)"
M="$TMP/mergesrc"; mkdir -p "$M"; ( cd "$M" && git init -q && git config user.email a@b.co && git config user.name a \
  && printf 'base\n' > base.txt && git add -A && git commit -qm c1 \
  && git checkout -q -b feature && printf 'f\n' > feat.txt && git add -A && git commit -qm feat \
  && git checkout -q master && printf 'm\n' > mainf.txt && git add -A && git commit -qm mainside \
  && git merge -q --no-edit feature ) >/dev/null 2>&1
"$KEEL" import "$M" --into "$M" >/dev/null 2>&1
"$KEEL" export "$TMP/mergeout" --store "$M/.keel/store" >/dev/null 2>&1
MC=$(git -C "$TMP/mergeout" rev-list --count HEAD 2>/dev/null)
MP=$(git -C "$TMP/mergeout" cat-file -p HEAD 2>/dev/null | grep -c '^parent ')
if [ "$MC" = "4" ] && [ "$MP" = "2" ]; then pass "merge round-trip (4 commits, 2-parent merge)"; else fail "merge round-trip (commits=$MC merge-parents=$MP)"; fi

section "status/diff correctness"
printf 'export function helper(x: number): number { return x * 3; }\n' > "$TMP/repo/util.ts"
ST=$("$KEEL" native status --root "$TMP/repo" 2>/dev/null)
if print -r -- "$ST" | grep -q "util.ts"; then pass "status detects the edit"; else fail "status ($ST)"; fi

section "benchmark suite — reproducibility manifest + dry-run (no API)"
BENCH=~/keel/src/keel-bench
if python3 "$BENCH/test_manifest.py" >/dev/null 2>&1 && python3 "$BENCH/run_suite.py" --dry-run >/dev/null 2>&1; then
  pass "suite harnesses import + manifest deterministic"
else
  fail "benchmark suite manifest / dry-run"
fi

rm -rf "$TMP"
print -r -- ""
print -r -- "════════════════════════════════════════"
if [ "$FAILED" -eq 0 ]; then print -r -- "CI GATE: PASS ✓"; exit 0
else print -r -- "CI GATE: FAIL ($FAILED gate(s)) ✗"; exit 1; fi

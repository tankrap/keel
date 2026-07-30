#!/bin/zsh
# keel benchmark suite — one entry point. Reproduces every headline number.
#
#   ./run_all.sh            # fast: the CI gate (build·tests·clippy·relevance·liveness·on-ramp)
#   ./run_all.sh --scale    # + storage/graph scale benches on cloned repos if present
#   ./run_all.sh --llm      # + the LLM flywheel eval (needs ~/.claude-token; costs tokens)
#   ./run_all.sh --all      # everything
#
# Pinned inputs (clone these to reproduce the scale numbers):
#   /Users/justin/keel-scale/linux    (torvalds/linux, --depth 1)   — 94.5k files, C
#   /Users/justin/keel-scale/vscode   (microsoft/vscode, --depth 1) — 9.3k files, TS
set -u
export PATH="/opt/homebrew/opt/node@22/bin:$PATH"
HERE="${0:A:h}"
RUST=~/keel/src/rust
LINUX=/Users/justin/keel-scale/linux
VSCODE=/Users/justin/keel-scale/vscode/src
want() { [[ " $* " == *" --all "* ]] && return 0; [[ " ${ARGS} " == *" $1 "* ]] && return 0; return 1 }
ARGS="$*"

print -r -- "═══ keel benchmark suite ═══"

print; print -r -- "[1/3] CI GATE (deterministic bars)"
"$HERE/ci_gate.sh" || { print -r -- "CI GATE FAILED — stopping"; exit 1; }

if want --scale; then
  print; print -r -- "[2/3] SCALE — storage + graph"
  cd "$RUST" && cargo build --release -q -p keel-store --example scale_bench --example system_bench -p keel-graph --example c_scale 2>/dev/null
  for repo in "$VSCODE" "$LINUX"; do
    [[ -d "$repo" ]] || { print -r -- "  (skip $repo — not cloned)"; continue; }
    print -r -- "  ── scale_bench $repo"
    "$RUST/target/release/examples/scale_bench" "$repo" 2>&1 | grep -E "files|throughput|keel store|latency|re-commit" | sed 's/^/    /'
  done
  if [[ -d "$VSCODE" ]]; then
    print -r -- "  ── relevance (system_bench, n=100) $VSCODE"
    "$RUST/target/release/examples/system_bench" "$VSCODE" 100 2>&1 | grep -E "cross-file|slice tok|latency|edges" | sed 's/^/    /'
  fi
  if [[ -d "$LINUX" ]]; then
    print -r -- "  ── C graph (c_scale) $LINUX"
    "$RUST/target/release/examples/c_scale" "$LINUX" 2>&1 | grep -E "files indexed|include edges|build time" | sed 's/^/    /'
  fi
else
  print; print -r -- "[2/3] SCALE — skipped (pass --scale)"
fi

if want --llm; then
  print; print -r -- "[3/3] FLYWHEEL (LLM eval)"
  python3 "$HERE/flywheel_live.py" 2>&1 | grep -E "retrieval|WITHOUT|WITH|lift" | sed 's/^/    /'
else
  print; print -r -- "[3/3] FLYWHEEL — skipped (pass --llm)"
fi

print; print -r -- "═══ suite complete ═══"

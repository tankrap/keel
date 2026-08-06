#!/usr/bin/env bash
# Reproducible performance + robustness benchmarks for keel's native core.
#
# These are the FREE benchmarks — no API key, no network, no LLM — so anyone can rerun them and check
# the numbers behind the README's speed/scale/robustness claims. (The paid convention-following
# flywheel suite lives in ../keel-bench and is run separately.)
#
#   ./run-benches.sh [SCALE_DIR]
#
# SCALE_DIR is a large real tree for the scale benchmark (ingest/status/GC). If omitted, it falls back
# to the local Cargo registry sources (~70k real files) when present, else skips the scale stage.
#
# Exit status is non-zero if any stage fails or the adversarial stress test reports a WEAKNESS, so
# this doubles as a CI-runnable smoke.
set -euo pipefail
cd "$(dirname "$0")"

SCALE_DIR="${1:-}"
if [[ -z "$SCALE_DIR" ]]; then
  # default: the crates.io source cache — a real, ~70k-file, multi-language tree already on disk
  SCALE_DIR="$(ls -d "$HOME"/.cargo/registry/src/index.crates.io-*/ 2>/dev/null | head -1 || true)"
fi

echo "════════════════════════════════════════════════════════════"
echo " keel native benchmarks · $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo " rustc: $(rustc --version 2>/dev/null || echo '?')"
echo "════════════════════════════════════════════════════════════"

echo; echo "▶ building release examples ..."
cargo build --release -q --examples -p keel-store -p keel-brief

echo; echo "########## object store — throughput & latency ##########"
cargo run --release -q -p keel-store --example bench

echo; echo "########## adversarial stress — core VCS (0 weaknesses expected) ##########"
stress_out="$(cargo run --release -q -p keel-store --example stress)"
echo "$stress_out"
if grep -q "WEAKNESS" <<<"$stress_out"; then
  echo "FAIL: stress reported a weakness" >&2
  exit 1
fi

echo; echo "########## stress round 2 — coordination / history / brief ##########"
stress2_out="$(cargo run --release -q -p keel-brief --example stress2)"
echo "$stress2_out"
if grep -q "WEAKNESS" <<<"$stress2_out"; then
  echo "FAIL: stress2 reported a weakness" >&2
  exit 1
fi

echo; echo "########## concurrent-writer ceiling ##########"
cargo run --release -q -p keel-store --example writers_bench

if [[ -n "$SCALE_DIR" && -d "$SCALE_DIR" ]]; then
  echo; echo "########## scale — ingest / status / GC on a large real tree ##########"
  echo "target: $SCALE_DIR"
  cargo run --release -q -p keel-store --example scale_bench -- "$SCALE_DIR"
else
  echo; echo "########## scale — SKIPPED (no SCALE_DIR and no Cargo registry src on disk) ##########"
fi

echo; echo "✓ all benchmarks completed"

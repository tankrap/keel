#!/usr/bin/env python3
"""Pure-logic self-tests for detection_bench.py — the JSON predicates and the scenario generators.

Runs with NO keel binary and NO API (the full scenario sweep in detection_bench.main() needs the keel
binary and is invoked manually, like the LLM benchmarks). This guards the harness's own correctness:
that a generator actually injects exactly one bug of the class it claims, and that the flag predicates
read the semantic-diff JSON shape correctly.

    python3 test_detection.py
"""
import random
import re

import detection_bench as D


def test_predicates_read_the_json_shape():
    assert D._added_flagged({"operator_anomalies": [{"reason": "operator < where 5/6 use <="}]})
    assert D._added_flagged({"groups": [{"anomalies": [{"reason": "literal 3 where 5/6 use 2"}]}]})
    assert not D._added_flagged({"operator_anomalies": [], "groups": [{"anomalies": []}]})
    assert D._removed_flagged({"removed": {"operator_anomalies": [{"reason": "x"}]}})
    assert not D._removed_flagged({"removed": {"operator_anomalies": [], "groups": []}})
    # a removed anomaly must NOT count as an added one, and vice versa
    assert not D._added_flagged({"removed": {"operator_anomalies": [{"reason": "x"}]}})
    assert D._total({"anomalies": 2, "removed": {"anomalies": 1}}) == 3
    assert D._total({}) == 0
    # _anom_texts collects the offending source line from BOTH operator and group anomalies
    scope = {
        "operator_anomalies": [{"text": "const r1 = a > lim;"}],
        "groups": [{"anomalies": [{"text": "const r2 = ok(v, 8);"}]}, {"anomalies": []}],
    }
    assert D._anom_texts(scope) == ["const r1 = a > lim;", "const r2 = ok(v, 8);"]
    assert D._anom_texts({}) == []
    print("  ✓ predicates read the semantic-diff JSON shape")


def _added_op_tokens(change, base):
    """The operator token in each ADDED `const rK = a <OP> lim && ...` line (base lines removed)."""
    new = [ln for ln in change.splitlines() if ln not in base.splitlines()]
    ops = []
    for ln in new:
        m = re.search(r"= \w+ (\S+) lim", ln)
        if m:
            ops.append(m.group(1))
    return ops


def test_op_flip_added_injects_exactly_one_minority_operator():
    rng = random.Random(0)
    for _ in range(50):
        base, change, (side, is_bug, site) = D.gen_op_flip_added(rng)
        assert (side, is_bug) == ("added", True)
        assert change.count(site) == 1, f"site marker must pick exactly one line: {site!r}"
        ops = _added_op_tokens(change, base)
        assert len(ops) >= 6
        counts = {o: ops.count(o) for o in set(ops)}
        # exactly two distinct operators: a majority and a single minority (the flip)
        assert len(counts) == 2, counts
        assert sorted(counts.values())[0] == 1, f"one flipped line only: {counts}"
    print("  ✓ op-flip-added injects exactly one minority operator")


def test_literal_smuggle_injects_one_minority_literal():
    rng = random.Random(0)
    for _ in range(50):
        base, change, exp = D.gen_literal_smuggle(rng)
        assert exp[:2] == ("added", True)
        assert change.count(exp[2]) == 1, f"site marker must pick one line: {exp[2]!r}"
        lits = [int(m) for m in re.findall(r"ok\(\w+, (\d+)\)", change)]
        counts = {v: lits.count(v) for v in set(lits)}
        assert len(counts) == 2 and sorted(counts.values())[0] == 1, counts
    print("  ✓ literal-smuggle injects exactly one minority literal")


def test_op_flip_removed_deletes_all_guards_one_odd():
    rng = random.Random(0)
    for _ in range(50):
        base, change, exp = D.gen_op_flip_removed(rng)
        assert exp[:2] == ("removed", True)
        assert base.count(exp[2]) == 1, f"site marker must pick one guard: {exp[2]!r}"
        # every guard line is gone in the change (a whole-guard-block deletion)
        assert "throw Error();" in base and "throw Error();" not in change
        ops = re.findall(r"if \(\w+ (\S+) max\)", base)
        counts = {o: ops.count(o) for o in set(ops)}
        assert len(counts) == 2 and sorted(counts.values())[0] == 1, counts
    print("  ✓ op-flip-removed deletes the guard block with one odd operator")


def test_clean_controls_carry_no_injected_bug():
    rng = random.Random(0)
    for gen in (D.gen_clean_uniform, D.gen_clean_varied_op, D.gen_clean_varied_lit):
        for _ in range(30):
            base, change, exp = gen(rng)
            assert exp[:2] == ("clean", False) and exp[2] is None
            assert change != base and change.startswith(base.splitlines()[0])
    # a varied-op control must use DISTINCT operators (no majority to flag)
    base, change, _ = D.gen_clean_varied_op(random.Random(3))
    ops = _added_op_tokens(change, base)
    assert len(set(ops)) == len(ops), f"varied-op should have no repeats: {ops}"
    print("  ✓ clean controls carry no injected bug (varied-op has no majority)")


def test_every_class_is_covered():
    assert set(D.CLASSES) == {
        "op-flip-added", "literal-smuggle", "op-flip-removed",
        "clean-uniform", "clean-varied-op", "clean-varied-lit",
    }
    print("  ✓ all six scenario classes registered")


if __name__ == "__main__":
    test_predicates_read_the_json_shape()
    test_op_flip_added_injects_exactly_one_minority_operator()
    test_literal_smuggle_injects_one_minority_literal()
    test_op_flip_removed_deletes_all_guards_one_odd()
    test_clean_controls_carry_no_injected_bug()
    test_every_class_is_covered()
    print("\ndetection_bench self-tests PASS")

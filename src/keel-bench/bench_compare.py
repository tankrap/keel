#!/usr/bin/env python3
"""Do two benchmark runs agree, and can we certify it? (NEW-1094)

The LLM parts of the suite are stochastic — the same pinned inputs won't reproduce byte-for-byte, so
"reproducible" for the measured rates cannot mean equal counts. It also cannot mean "a significance
test failed to find a difference": *absence of evidence is not evidence of reproduction*, and at this
suite's sample sizes (12–20 scenarios × 4 trials = 48–80 samples/condition) a plain two-proportion
test is so underpowered it would accept a real 15-point regression most of the time. That would give
false confidence.

So certification here is an EQUIVALENCE test (TOST): the two runs reproduce *within a margin δ* only
if the (1−α) confidence interval for their difference lies entirely inside (−δ, +δ). Because
equivalence is the thing we must *reject the null to conclude*, low power makes this CONSERVATIVE —
too few samples means "inconclusive," never a false "reproduced." Separately, we still flag a genuine
REGRESSION with a Holm-corrected two-proportion test (correcting the family-wise error across the many
conditions, so a run that truly reproduced isn't failed by chance ~40% of the time).

Three outcomes, manifest-gated (differing `manifest_sha256` ⇒ different inputs ⇒ not comparable):
  - EQUIVALENT   — every condition's difference CI ⊂ (−δ, +δ). The strong, honest "it reproduced."
  - REGRESSION   — some condition differs significantly (Holm-corrected). A real change.
  - INCONCLUSIVE — neither: the samples can't certify equivalence within δ (underpowered). The output
                   prints the achievable resolution so you know how many more trials you'd need.

δ defaults to 20 percentage points — not a quality bar but the *floor dictated by the sample size*:
at n=48 and a 50% base rate even two identical runs can only be certified within ≈±20pp at 95%.
Tighten δ with --margin once you raise the trial count. No API, no token: it reads two JSON files.

    python3 run_suite.py --compare run_a.json run_b.json [--margin PP]
"""
import json
import math

Z_DEFAULT = 1.96          # 95%; overridden by the reports' run.wilson_z when they agree
MARGIN_DEFAULT = 0.20     # equivalence margin (fraction) — the sample-size floor, see module doc


def _phi(x):
    """Standard normal CDF via erf (no scipy)."""
    return 0.5 * (1.0 + math.erf(x / math.sqrt(2.0)))


def _two_sided_p(z):
    return 2.0 * (1.0 - _phi(abs(z)))


def _analyze(k1, n1, k2, n2, z_crit, margin):
    """One condition. Returns the difference, its significance p-value (pooled two-proportion, for the
    regression test), and the TOST equivalence verdict (unpooled CI ⊂ ±margin, for certification)."""
    p1, p2 = k1 / n1, k2 / n2
    d = p1 - p2

    # Significance (H0: p1==p2) uses the POOLED variance — the right estimate under the null.
    pool = (k1 + k2) / (n1 + n2)
    se_pooled = math.sqrt(pool * (1 - pool) * (1 / n1 + 1 / n2)) if 0 < pool < 1 else 0.0
    if se_pooled == 0:            # pool ∈ {0,1} ⇒ both all-fail or all-pass ⇒ p1==p2, no difference
        z_diff, p_val = 0.0, 1.0
    else:
        z_diff = d / se_pooled
        p_val = _two_sided_p(z_diff)

    # Equivalence (TOST) uses the UNPOOLED variance — we are NOT assuming the rates are equal. The run
    # reproduces within margin iff the whole (1−α) CI for d sits inside (−margin, +margin).
    se_unp = math.sqrt(p1 * (1 - p1) / n1 + p2 * (1 - p2) / n2)
    ci_half = z_crit * se_unp
    equivalent = (abs(d) + ci_half) <= margin

    return {"d": d, "p_val": p_val, "z_diff": z_diff, "ci_half": ci_half, "equivalent": equivalent}


def _holm_reject(pvals, alpha):
    """Holm step-down: returns a bool list (aligned to input order) of which p-values are rejected at
    family-wise α. Controls the chance of a spurious 'regression' across the many conditions."""
    m = len(pvals)
    reject = [False] * m
    for rank, i in enumerate(sorted(range(m), key=lambda j: pvals[j])):
        if pvals[i] <= alpha / (m - rank):
            reject[i] = True
        else:
            break  # step-down: once one fails, all larger p-values also fail
    return reject


def _manifest_sha(report):
    return (report.get("manifest") or {}).get("manifest_sha256")


def _keel_commit(report):
    return ((report.get("environment") or {}).get("keel") or {}).get("git_commit")


def _wilson_z(report):
    return ((report.get("run") or {}).get("wilson_z"))


def _by_id(report):
    out = {}
    for b in report.get("benchmarks", []):
        bid = b.get("id")
        if bid is not None:
            out[bid] = b
    return out


def _counts(cond):
    """(correct, total) from a condition dict, or None if the fields are missing/blank."""
    if not isinstance(cond, dict):
        return None
    k, n = cond.get("correct"), cond.get("total")
    if not isinstance(k, int) or not isinstance(n, int) or n <= 0 or k < 0 or k > n:
        return None
    return k, n


def compare(report_a, report_b, margin=MARGIN_DEFAULT, z=None):
    """Compare two report dicts. `z` defaults to the reports' shared run.wilson_z (else 1.96). Returns
    a result dict with per-condition analysis and an overall verdict in
    {"equivalent","regression","inconclusive","manifest_mismatch","no_data"}."""
    ma, mb = _manifest_sha(report_a), _manifest_sha(report_b)
    manifest_match = ma is not None and ma == mb
    ka, kb = _keel_commit(report_a), _keel_commit(report_b)

    # Prefer the reports' own Wilson z (and only trust it when both agree), so --compare uses the same
    # confidence level the reports were built at rather than a hardcoded constant.
    za, zb = _wilson_z(report_a), _wilson_z(report_b)
    if z is None:
        z = za if (za is not None and za == zb) else Z_DEFAULT
    alpha = _two_sided_p(z)  # the significance level implied by the CI's z (≈0.05 at z=1.96)

    a_by, b_by = _by_id(report_a), _by_id(report_b)
    shared = sorted(set(a_by) & set(b_by))
    unmatched = sorted(set(a_by) ^ set(b_by))

    conditions = []
    for bid in shared:
        for cond in ("without", "with"):
            ca, cb = _counts(a_by[bid].get(cond)), _counts(b_by[bid].get(cond))
            if ca is None or cb is None:
                continue
            a = _analyze(ca[0], ca[1], cb[0], cb[1], z, margin)
            conditions.append({
                "id": bid, "condition": cond,
                "a": list(ca), "b": list(cb),
                "a_pct": round(100 * ca[0] / ca[1], 1), "b_pct": round(100 * cb[0] / cb[1], 1),
                "diff_pct": round(100 * a["d"], 1),
                "ci_half_pct": round(100 * a["ci_half"], 1),
                "p_val": round(a["p_val"], 4),
                "equivalent": a["equivalent"],
            })

    # Holm-correct the regression tests across all conditions (family-wise α).
    reject = _holm_reject([c["p_val"] for c in conditions], alpha) if conditions else []
    for c, r in zip(conditions, reject):
        c["regressed"] = bool(r)

    if not manifest_match:
        verdict = "manifest_mismatch"
    elif not conditions:
        verdict = "no_data"
    elif any(c["regressed"] for c in conditions):
        verdict = "regression"
    elif all(c["equivalent"] for c in conditions):
        verdict = "equivalent"
    else:
        verdict = "inconclusive"

    return {
        "verdict": verdict,
        "manifest_a": ma, "manifest_b": mb, "manifest_match": manifest_match,
        "keel_a": ka, "keel_b": kb, "keel_match": (ka == kb),
        "z": z, "alpha": round(alpha, 4), "margin_pct": round(100 * margin, 1),
        "conditions": conditions, "unmatched": unmatched,
        # kept for callers/back-compat: the strong PASS is exactly the equivalence verdict
        "reproduces_within_noise": verdict == "equivalent",
    }


def load_report(path):
    with open(path) as f:
        return json.load(f)


if __name__ == "__main__":
    import sys
    if len(sys.argv) != 3:
        print("usage: bench_compare.py REPORT_A.json REPORT_B.json", file=sys.stderr)
        raise SystemExit(2)
    print(json.dumps(compare(load_report(sys.argv[1]), load_report(sys.argv[2])), indent=2))

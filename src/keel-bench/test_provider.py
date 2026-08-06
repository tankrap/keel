#!/usr/bin/env python3
"""Self-test for the LLM transport layer (--provider / BENCH_PROVIDER). No API, no token.

Locks the provider wiring: each transport builds the right URL, auth header and wire format, maps the
pinned model ids correctly, and parses the matching response shape — including surfacing an error
envelope instead of silently returning "". `build_request` / `parse_response` are pure, so this runs
offline. Run:

    python3 test_provider.py
"""
import json
import pathlib
import sys

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import bench_common as bc  # noqa: E402


def main():
    fails = 0

    def check(cond, msg):
        nonlocal fails
        print(f"  {'✓' if cond else '✗'} {msg}")
        if not cond:
            fails += 1

    # ── anthropic (default) ─────────────────────────────────────────────
    bc.PROVIDER = "anthropic"
    url, headers, body = bc.build_request("SYS", "USER", bc.SOLVER, 1600, "KEY")
    d = json.loads(body)
    check(url == bc.API_URL, "anthropic → api.anthropic.com/v1/messages")
    check(headers.get("x-api-key") == "KEY", "anthropic → x-api-key auth")
    check(headers.get("anthropic-version") == bc.API_VERSION, "anthropic → version header")
    check(d["model"] == "claude-opus-4-8", "anthropic → pinned model id verbatim")
    check(d.get("system") == "SYS" and d["messages"] == [{"role": "user", "content": "USER"}],
          "anthropic → system top-level + single user message")
    check("temperature" not in d, "no temperature field is ever sent (opus-4-8 forces temp=1)")
    # response parse
    txt = bc.parse_response({"content": [{"type": "text", "text": "hi"},
                                         {"type": "thinking", "text": "ignore"}]})
    check(txt == "hi", "anthropic → concatenates only text blocks")
    try:
        bc.parse_response({"type": "error", "error": {"message": "boom"}})
        check(False, "anthropic → error envelope raises")
    except RuntimeError:
        check(True, "anthropic → error envelope raises")

    # ── openrouter ──────────────────────────────────────────────────────
    bc.PROVIDER = "openrouter"
    url, headers, body = bc.build_request("SYS", "USER", bc.SOLVER, 1600, "KEY")
    d = json.loads(body)
    check(url == bc.OPENROUTER_URL, "openrouter → openrouter.ai/api/v1/chat/completions")
    check(headers.get("Authorization") == "Bearer KEY", "openrouter → Bearer auth")
    check(d["model"] == "anthropic/claude-opus-4.8", "openrouter → solver mapped to OR slug")
    j = bc.build_request("SYS", "USER", bc.JUDGE, 600, "K")[2]
    check(json.loads(j)["model"] == "anthropic/claude-sonnet-5", "openrouter → judge mapped to OR slug")
    check(d["messages"] == [{"role": "system", "content": "SYS"}, {"role": "user", "content": "USER"}],
          "openrouter → system+user as OpenAI-style messages")
    check("system" not in d, "openrouter → no top-level system (it lives in messages)")
    # response parse
    txt = bc.parse_response({"choices": [{"message": {"content": "yo"}}]})
    check(txt == "yo", "openrouter → reads choices[0].message.content")
    try:
        bc.parse_response({"error": {"message": "no credit"}})
        check(False, "openrouter → error envelope raises")
    except RuntimeError:
        check(True, "openrouter → error envelope raises")

    # ── key file selection (no read — just the mapping) ─────────────────
    check(bc.KEY_FILE["anthropic"] == ".claude-token" and bc.KEY_FILE["openrouter"] == ".openrouter",
          "each provider reads its own key file")

    print(f"\n{'PASS' if not fails else 'FAIL'} — provider wiring ({fails} failure(s))")
    return 1 if fails else 0


if __name__ == "__main__":
    raise SystemExit(main())

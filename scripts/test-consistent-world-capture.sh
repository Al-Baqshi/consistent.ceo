#!/usr/bin/env bash
#
# Offline smoke test for scripts/consistent-world-capture. Exercises argument mapping,
# stdin capture, JSON escaping, and `run` mode via --dry-run so it never needs a
# running gateway. Run with: bash scripts/test-consistent-world-capture.sh
#
# This is a lightweight, conservative test — not an integration test. The gateway
# side is covered by cargo tests (`external_capture_*` in gateway_test.rs).

set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
CAP="$HERE/consistent-world-capture"
FAILED=0

pass() { printf 'ok   - %s\n' "$1"; }
fail() { printf 'FAIL - %s\n' "$1"; FAILED=1; }

assert_contains() {
  # assert_contains "<haystack>" "<needle>" "<name>"
  case "$1" in
    *"$2"*) pass "$3" ;;
    *) fail "$3 (missing: $2)"; printf '       in: %s\n' "$1" >&2 ;;
  esac
}

# JSON validity check, using python3 when available (best-effort).
assert_json() {
  if command -v python3 >/dev/null 2>&1; then
    if printf '%s' "$1" | python3 -c 'import json,sys; json.load(sys.stdin)' 2>/dev/null; then
      pass "$2"
    else
      fail "$2 (invalid JSON)"; printf '       body: %s\n' "$1" >&2
    fi
  else
    printf 'skip - %s (python3 not available)\n' "$2"
  fi
}

# 1. --help works and mentions usage.
HELP="$(bash "$CAP" --help)"
assert_contains "$HELP" "consistent-world-capture" "help output shows program name"
assert_contains "$HELP" "run [options]" "help documents run mode"

# 2. Simple capture with --text maps fields and is valid JSON.
BODY="$(bash "$CAP" --source claude_code --text 'ship it' --auto-route --dry-run)"
assert_contains "$BODY" '"source":"claude_code"' "source flag mapped"
assert_contains "$BODY" '"raw_text":"ship it"' "text flag mapped"
assert_contains "$BODY" '"auto_route":true' "auto-route flag mapped"
assert_json "$BODY" "simple body is valid JSON"

# 3. stdin is captured when --text is omitted.
BODY="$(printf 'line one\nline two' | bash "$CAP" --source shell --dry-run)"
assert_contains "$BODY" '"raw_text":"line one\nline two"' "stdin captured with newline escaped"
assert_json "$BODY" "stdin body is valid JSON"

# 4. JSON escaping handles quotes and backslashes.
BODY="$(bash "$CAP" --source shell --text 'a "quote" and \ slash' --dry-run)"
assert_contains "$BODY" '\"quote\"' "double quotes escaped"
assert_contains "$BODY" '\\ slash' "backslash escaped"
assert_json "$BODY" "escaped body is valid JSON"

# 5. Optional hint flags land in the body.
BODY="$(bash "$CAP" --source browser --text note --repo-url https://github.com/acme/widgets \
  --url https://acme.com --entity-id ent-1 --external-id x1 --dry-run)"
assert_contains "$BODY" '"repo_url":"https://github.com/acme/widgets"' "repo-url mapped"
assert_contains "$BODY" '"url":"https://acme.com"' "url mapped"
assert_contains "$BODY" '"entity_id":"ent-1"' "entity-id mapped"
assert_contains "$BODY" '"external_id":"x1"' "external-id mapped"

# 6. run mode captures command output, exit code, and metadata; returns cmd exit.
BODY="$(bash "$CAP" run --source shell --title tests --dry-run -- printf 'out%s' 123)"
RC=$?
assert_contains "$BODY" '"raw_text":"out123"' "run mode captured command stdout"
assert_contains "$BODY" '"exitCode":0' "run mode recorded exit code"
assert_contains "$BODY" '"command":"printf out%s 123"' "run mode recorded command"
assert_contains "$BODY" '"title":"tests"' "run mode title mapped"
assert_json "$BODY" "run-mode body is valid JSON"
[ "$RC" -eq 0 ] && pass "run mode returns command exit code (0)" || fail "run mode exit code passthrough"

# 7. run mode propagates a non-zero command exit code.
bash "$CAP" run --source shell --dry-run -- sh -c 'exit 7' >/dev/null
[ "$?" -eq 7 ] && pass "run mode propagates non-zero exit code (7)" || fail "non-zero exit passthrough"

# 8. Empty text is rejected.
if printf '' | bash "$CAP" --source shell --dry-run >/dev/null 2>&1; then
  fail "empty text should be rejected"
else
  pass "empty text is rejected"
fi

# 9. Options requiring a value fail clearly when the value is missing or is
#    another option. These must exit non-zero (and not hang).
assert_missing_value() {
  # assert_missing_value "<flag-for-message>" "<name>" -- <cmd args...>
  local expect_flag="$1" name="$2"
  shift 3 # drop expect_flag, name, and the literal '--'
  local err rc
  err="$(bash "$CAP" "$@" </dev/null 2>&1 >/dev/null)"
  rc=$?
  if [ "$rc" -ne 0 ] && printf '%s' "$err" | grep -q -- "$expect_flag requires a value"; then
    pass "$name"
  else
    fail "$name (rc=$rc, err=$err)"
  fi
}

assert_missing_value "--source" "--source with no value fails" -- --source
assert_missing_value "--source" "--source followed by another option fails" -- --source --text hi
assert_missing_value "--gateway" "--gateway with no value fails" -- --gateway
assert_missing_value "--entity-id" "--entity-id with no value fails" -- --entity-id

# 10. '--' still terminates options for run mode after valid value flags.
BODY="$(bash "$CAP" run --source shell --dry-run -- echo hi)"
RC=$?
assert_contains "$BODY" '"raw_text":"hi"' "run mode with -- captures command output"
assert_contains "$BODY" '"source":"shell"' "run mode with -- keeps source"
[ "$RC" -eq 0 ] && pass "run mode with -- returns exit 0" || fail "run mode with -- exit code"

if [ "$FAILED" -eq 0 ]; then
  printf '\nAll consistent-world-capture smoke tests passed.\n'
  exit 0
else
  printf '\nSome consistent-world-capture smoke tests FAILED.\n' >&2
  exit 1
fi

#!/usr/bin/env bash
# Repo health: the drift checks that would have caught a two-month unlanded
# branch, a stale roadmap describing an abandoned plan, and a shipped doc
# promising a pipeline whose stages are placeholder functions.
#
# Every check here exists because it already went wrong once. Thresholds are
# deliberately loose — this is a smoke alarm, not a linter. A WARN means look
# at it this week; a BREACH means stop and deal with it.
#
# Usage:
#   scripts/repo-health.sh              # fast checks only (~2s, no network)
#   scripts/repo-health.sh --full       # + cargo check, clippy, frontend build
#   scripts/repo-health.sh --ci         # machine-readable, non-zero on BREACH
#
# Exit: 0 clean · 1 warnings · 2 breach

set -uo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null)" || { echo "not a git repo"; exit 2; }

# ---- thresholds -------------------------------------------------------------
MAX_MAIN_AGE_DAYS=14        # main should absorb work at least fortnightly
MAX_COMMITS_AHEAD=25        # a branch bigger than this is not reviewable
MAX_UNCOMMITTED_LINES=800   # more than this is a feature nobody has saved
MAX_UNTRACKED_FILES=10
MAX_DOC_LAG_DAYS=30         # planning docs vs. the code they describe
STUB_LINE_LIMIT=15          # a "module" this small is a placeholder
MAX_TASK_WRITE_SITES=38     # ratchet: direct tasks.write() outside state.rs, may only fall

FULL=0; CI=0
for a in "$@"; do
  case "$a" in
    --full) FULL=1 ;;
    --ci)   CI=1 ;;
    -h|--help) sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown flag: $a" >&2; exit 2 ;;
  esac
done

warns=0; breaches=0
if [ -t 1 ] && [ "$CI" -eq 0 ]; then
  B=$'\033[1m'; DIM=$'\033[2m'; R=$'\033[31m'; Y=$'\033[33m'; G=$'\033[32m'; N=$'\033[0m'
else
  B=""; DIM=""; R=""; Y=""; G=""; N=""
fi

ok()      { printf "  ${G}ok${N}      %s\n" "$1"; }
warn()    { printf "  ${Y}WARN${N}    %s\n" "$1"; warns=$((warns+1)); }
breach()  { printf "  ${R}BREACH${N}  %s\n" "$1"; breaches=$((breaches+1)); }
note()    { printf "          ${DIM}%s${N}\n" "$1"; }
section() { printf "\n${B}%s${N}\n" "$1"; }

days_since_commit() {  # $1 = ref or path; echoes whole days, or -1 if unknown
  local ts now
  # A ref and a path are both valid arguments to git log but mean opposite
  # things, so resolve which one this is before asking.
  if git rev-parse --verify --quiet "$1^{commit}" >/dev/null 2>&1; then
    ts=$(git log -1 --format=%ct "$1" 2>/dev/null)
  else
    ts=$(git log -1 --format=%ct -- "$1" 2>/dev/null)
  fi
  [ -z "$ts" ] && { echo -1; return; }
  now=$(date +%s)
  echo $(( (now - ts) / 86400 ))
}

printf "${B}repo health${N}  %s  ${DIM}%s${N}\n" \
  "$(git rev-parse --abbrev-ref HEAD)" "$(git rev-parse --short HEAD)"

# ---- 1. unlanded work -------------------------------------------------------
section "1. Unlanded work"
branch=$(git rev-parse --abbrev-ref HEAD)
main_age=$(days_since_commit main)

if [ "$main_age" -lt 0 ]; then
  warn "no local 'main' branch to compare against"
elif [ "$main_age" -gt "$MAX_MAIN_AGE_DAYS" ]; then
  breach "main has not moved in ${main_age} days (limit ${MAX_MAIN_AGE_DAYS})"
  note "work that never lands is work that only exists on one machine"
else
  ok "main last moved ${main_age} days ago"
fi

if [ "$branch" != "main" ]; then
  ahead=$(git rev-list --count main.."$branch" 2>/dev/null || echo 0)
  if [ "$ahead" -gt "$MAX_COMMITS_AHEAD" ]; then
    breach "${branch} is ${ahead} commits ahead of main (limit ${MAX_COMMITS_AHEAD})"
    note "split it or land it — a branch this size never gets reviewed"
  elif [ "$ahead" -gt 0 ]; then
    ok "${branch} is ${ahead} commits ahead of main"
  else
    ok "${branch} is level with main"
  fi
fi

# ---- 2. uncommitted volume --------------------------------------------------
section "2. Uncommitted volume"
changed=$(git diff --numstat 2>/dev/null | awk '{a+=$1; d+=$2} END {print (a+d)+0}')
untracked=$(git ls-files --others --exclude-standard | wc -l | tr -d ' ')

if [ "$changed" -gt "$MAX_UNCOMMITTED_LINES" ]; then
  breach "${changed} uncommitted lines (limit ${MAX_UNCOMMITTED_LINES})"
  note "commit in feature-sized pieces; a WIP tree is not a backup"
elif [ "$changed" -gt 0 ]; then
  ok "${changed} uncommitted lines"
else
  ok "working tree clean"
fi

if [ "$untracked" -gt "$MAX_UNTRACKED_FILES" ]; then
  warn "${untracked} untracked files (limit ${MAX_UNTRACKED_FILES})"
  git ls-files --others --exclude-standard | head -5 | sed 's/^/          /'
fi

# ---- 3. CI on main ----------------------------------------------------------
section "3. CI on main"
if command -v gh >/dev/null 2>&1; then
  ci=$(gh run list --branch main --limit 1 --json status,conclusion,displayTitle 2>/dev/null)
  if [ -n "$ci" ] && [ "$ci" != "[]" ]; then
    st=$(printf '%s' "$ci" | sed -n 's/.*"status":"\([^"]*\)".*/\1/p')
    cc=$(printf '%s' "$ci" | sed -n 's/.*"conclusion":"\([^"]*\)".*/\1/p')
    case "$cc" in
      success)  ok "last run on main: success" ;;
      "")       ok "run in progress (${st})" ;;
      *)        breach "last run on main: ${cc}"
                note "a red main means nobody can trust a green local build" ;;
    esac
  else
    warn "no CI runs found for main"
  fi
else
  note "gh not installed — skipping"
fi

# ---- 3b. toolchain drift ----------------------------------------------------
# CI resolves `stable` via rustup at run time. A local toolchain behind that
# cannot see newer lints, so `clippy` passes here and fails there — which is
# exactly how a red main went unnoticed on 9 Sep (1.97 local vs 1.98 in CI).
section "3b. Toolchain"
local_rust=$(rustc --version 2>/dev/null | awk '{print $2}')
pinned=$(sed -n 's/^ *channel *= *"\([^"]*\)".*/\1/p' rust-toolchain.toml 2>/dev/null | head -1)

if [ -z "$local_rust" ]; then
  warn "rustc not on PATH"
elif [ -z "$pinned" ]; then
  warn "rust-toolchain.toml pins no channel"
elif [ "$pinned" = "stable" ] || [ "$pinned" = "nightly" ]; then
  warn "rust-toolchain.toml tracks '${pinned}' rather than a version"
  note "a moving channel is how local-green/CI-red happened; pin it"
# A pin of "1.97" is satisfied by any 1.97.x, so compare on the pinned prefix
# rather than demanding an exact string match.
elif [ "${local_rust#"$pinned"}" != "$local_rust" ]; then
  ok "rustc ${local_rust} matches the pinned ${pinned}"
else
  breach "rustc ${local_rust} != pinned ${pinned} — CI judges with a different compiler"
  note "clippy can pass here and fail there; install ${pinned} or bump the pin"
fi

# ---- 4. stub drift ----------------------------------------------------------
# A module small enough to be a placeholder, in a crate the docs describe as
# delivered, is the failure that let four of five Architect stages sit unbuilt
# for months while VISION.md called the pipeline non-negotiable.
section "4. Stub drift"
stubs=""
while IFS= read -r f; do
  [ -f "$f" ] || continue
  lines=$(wc -l < "$f" | tr -d ' ')
  if [ "$lines" -le "$STUB_LINE_LIMIT" ] &&
     grep -qE "placeholder|todo!\(|unimplemented!\(|not implemented" "$f" 2>/dev/null; then
    stubs="${stubs}${f} (${lines} lines)\n"
  fi
done < <(git ls-files 'crates/**/*.rs')

if [ -n "$stubs" ]; then
  n=$(printf "%b" "$stubs" | grep -c . )
  warn "${n} placeholder module(s) still shipping"
  printf "%b" "$stubs" | sed 's/^/          /'
  note "either build them or stop describing them as delivered in docs/"
else
  ok "no placeholder modules"
fi

# ---- 5. doc drift -----------------------------------------------------------
# The roadmap described a plan nobody was following. That is worse than no
# roadmap, because it hides the fact that a decision was made.
section "5. Doc drift"
code_age=$(days_since_commit crates)
for d in docs/ROADMAP.md STATUS.md docs/VISION.md ARCHITECTURE.md; do
  [ -f "$d" ] || { warn "$d is missing"; continue; }
  age=$(days_since_commit "$d")
  lag=$(( age - code_age ))
  if [ "$lag" -gt "$MAX_DOC_LAG_DAYS" ]; then
    breach "$d is ${age}d old — ${lag}d behind the code it describes"
  elif [ "$age" -gt "$MAX_DOC_LAG_DAYS" ]; then
    warn "$d is ${age}d old"
  else
    ok "$d current (${age}d)"
  fi
done

# ---- 6. dead surfaces -------------------------------------------------------
# Every route the sidebar cannot reach is a promise the product does not keep.
section "6. Dead surfaces"
dead=""
if [ -f sveltfront/src/lib/components/Sidebar.svelte ]; then
  while IFS= read -r page; do
    route="/$(dirname "${page#sveltfront/src/routes/}")"
    [ "$route" = "/." ] && continue
    case "$route" in
      */\[*\]*) continue ;;                    # detail pages are reached from a parent
      /login|/download|/about|/settings) continue ;;  # reached by redirect or the bottom rail
    esac
    # Reachability means *any* internal navigation, not just the sidebar. This
    # check used to grep the sidebar alone, and so reported `/console` as dead
    # when it is reached from the dashboard and the agents page via
    # `goto('/console?directorId=…')` — a sweep acting on that would have
    # deleted a working screen. Match a leading quote and either a closing
    # quote or a `?`, so query-string links count and `/data` never matches
    # `/database`.
    grep -rqE "['\"\`]${route}([?'\"\`]|\\$)" sveltfront/src --include='*.svelte' --include='*.ts' \
      || dead="${dead}${route} "
  done < <(git ls-files 'sveltfront/src/routes/**/+page.svelte')
fi
compat=$(grep -roh '"/api/compat[^"]*"' crates/gateway/src --include='*.rs' 2>/dev/null | sort -u | wc -l | tr -d ' ')

if [ -n "$dead" ]; then
  warn "routes with no internal link: ${dead}"
  note "delete them, or link them — an orphan page is a maintenance tax"
else
  ok "every route is reachable"
fi
[ "$compat" -gt 0 ] && warn "${compat} /api/compat/* endpoints (no known caller since the iOS app was removed)"

# ---- 7. unsupervised background loops ---------------------------------------
# A background loop spawned with a bare `tokio::spawn` whose handle is dropped
# dies silently on its first panic: tokio ends the task, nothing logs at loop
# level, and /health keeps answering 200. Nine loops shipped that way, which
# meant a panicking orchestrator stopped all unattended execution and looked
# exactly like an empty queue. Loops go through `supervisor::spawn_supervised`.
section "7. Loop supervision"
unsup=$(grep -nE '^\s*(let _[a-z_]* = )?tokio::spawn\(' crates/gateway/src/main.rs 2>/dev/null \
  | grep -vE 'sleep|open_url_in_browser|notify_failure' | wc -l | tr -d ' ')
bare_let=$(grep -cE '^\s*let _[a-z_]*(handle|_h) = tokio::spawn\(' crates/gateway/src/main.rs 2>/dev/null | tr -d ' ')
supervised=$(grep -c 'spawn_supervised(' crates/gateway/src/main.rs 2>/dev/null | tr -d ' ')

if [ "$bare_let" -gt 0 ]; then
  breach "${bare_let} background loop(s) spawned with a discarded handle in main.rs"
  note "use supervisor::spawn_supervised — a dropped JoinHandle means a silent death on panic"
  grep -nE '^\s*let _[a-z_]*(handle|_h) = tokio::spawn\(' crates/gateway/src/main.rs | sed 's/^/          /'
elif [ "$supervised" -lt 1 ]; then
  breach "no supervised loops found in main.rs — did spawn_supervised get removed?"
else
  ok "${supervised} background loops supervised, 0 with a discarded handle"
  [ "$unsup" -gt 0 ] && note "${unsup} one-shot tokio::spawn call(s) remain, which is fine — loops are the risk"
fi

# ---- 8. task mutation ---------------------------------------------------------
# Two patterns caused real defects and are now unrepresentable in production code.
#
# Writing a task by saved index: the orchestrator collected candidate positions
# under a read guard, dropped it, awaited Surreal I/O, then wrote `tasks[i]`. Any
# push or `retain` in between shifted the indices, so the write landed on a
# different task — hijacking the wrong one into an unattended run, or panicking on
# an out-of-bounds index. Re-find by id inside the guard: `ControlState::update_task`.
#
# Assigning `task.status` directly: skips `set_task_status_by_{human,loop}`, so
# `status_changed_at` and `completed_at` stop tracking reality (the streak and
# per-day counts are computed from them), and skips the loop-side guard that stops
# a finishing run from overwriting a human's cancel.
#
# Test fixtures legitimately do both, so only production lines count. In this repo
# inline `#[cfg(test)]` modules always sit at the end of a file, so everything from
# a file's first `#[cfg(test)]` to EOF is test code.
section "8. Task mutation"

prod_grep() {  # $1 = ERE; rest = files. Prints FILE:LINE: text for production hits.
  local re="$1"; shift
  # Line comments are skipped: prose about a pattern is not the pattern, and these
  # checks are explained in comments that would otherwise match themselves.
  awk -v re="$re" '
    FNR==1 { intest=0 }
    /#\[cfg\(test\)\]/ { intest=1 }
    /^[[:space:]]*\/\// { next }
    !intest && $0 ~ re { print FILENAME":"FNR": "$0 }
  ' "$@"
}

# --others so a file added but not yet committed is still checked: that is exactly
# when the author wants to hear about it, and `ls-files` alone would stay silent.
gw_files=$(git ls-files --cached --others --exclude-standard \
  'crates/gateway/src/*.rs' 'crates/gateway/src/**/*.rs' | grep -v 'src/state.rs$')
if [ -n "$gw_files" ]; then
  # shellcheck disable=SC2086
  raw_status=$(prod_grep '(^|[^a-zA-Z_])(t|task)\.status = [a-zA-Z_:]*TaskStatus::' $gw_files)
  # shellcheck disable=SC2086
  idx_write=$(prod_grep 'tasks\[[a-z_0-9]+\]\.[a-z_]+ = ' $gw_files)
  raw_n=$(printf '%s' "$raw_status" | grep -c . || true)
  idx_n=$(printf '%s' "$idx_write" | grep -c . || true)
  # shellcheck disable=SC2086
  write_n=$(prod_grep 'tasks\.write\(\)' $gw_files | grep -c . || true)

  if [ "$idx_n" -gt 0 ]; then
    breach "${idx_n} task write(s) by index outside state.rs"
    note "the index is stale the moment the read guard drops — use ControlState::update_task"
    printf '%s\n' "$idx_write" | sed 's/^/          /'
  else
    ok "no task writes by saved index"
  fi

  if [ "$raw_n" -gt 0 ]; then
    breach "${raw_n} direct task.status assignment(s) outside state.rs"
    note "use set_task_status_by_human or set_task_status_by_loop — direct writes skip the timestamps and the cancel guard"
    printf '%s\n' "$raw_status" | sed 's/^/          /'
  else
    ok "every task status change goes through a stamping setter"
  fi

  # A ratchet, not a rule: these are mostly already correct (write guard, then
  # re-find by id), so the number is a consolidation target for the state.rs split
  # rather than a defect count. It may only go down.
  if [ "$write_n" -gt "$MAX_TASK_WRITE_SITES" ]; then
    breach "${write_n} tasks.write() sites outside state.rs (ratchet: ${MAX_TASK_WRITE_SITES})"
    note "new direct write sites should go through ControlState::update_task instead"
  elif [ "$write_n" -lt "$MAX_TASK_WRITE_SITES" ]; then
    ok "${write_n} tasks.write() sites outside state.rs — below the ${MAX_TASK_WRITE_SITES} ratchet, lower it"
  else
    ok "${write_n} tasks.write() sites outside state.rs (at the ratchet)"
  fi
fi

# ---- 9. build gates (--full) ------------------------------------------------
if [ "$FULL" -eq 1 ]; then
  section "9. Build gates"
  if cargo check --workspace --all-targets >/tmp/rh-check.log 2>&1; then
    ok "cargo check"
  else
    breach "cargo check failed"; tail -15 /tmp/rh-check.log | sed 's/^/          /'
  fi
  if cargo clippy --workspace --all-targets -- -D warnings >/tmp/rh-clippy.log 2>&1; then
    ok "cargo clippy -D warnings"
  else
    breach "clippy failed (CI gates on this)"
    grep -E "^(error|warning)" /tmp/rh-clippy.log | head -10 | sed 's/^/          /'
  fi
  if (cd sveltfront && npm run build >/tmp/rh-web.log 2>&1); then
    ok "frontend build"
  else
    breach "frontend build failed"; tail -10 /tmp/rh-web.log | sed 's/^/          /'
  fi
else
  note ""
  note "run with --full to also gate on cargo check, clippy and the frontend build"
fi

# ---- verdict ----------------------------------------------------------------
printf "\n${B}%s${N}\n" "verdict"
if [ "$breaches" -gt 0 ]; then
  printf "  ${R}%d breach(es), %d warning(s)${N}\n\n" "$breaches" "$warns"; exit 2
elif [ "$warns" -gt 0 ]; then
  printf "  ${Y}%d warning(s)${N}\n\n" "$warns"; exit 1
else
  printf "  ${G}clean${N}\n\n"; exit 0
fi

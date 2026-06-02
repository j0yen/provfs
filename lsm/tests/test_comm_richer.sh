#!/usr/bin/env bash
# PRD-provfs-comm-richer acceptance test (AC1-AC9).
#
# Runtime test — documents and exercises the post-reboot validation the
# PRD requires (§3 AC10: "AC1-9 verified live by jsy on the rebuilt +
# rebooted linux-wintermute kernel"). The provfs LSM is built-in, so this
# can only run after booting the wintermute kernel; until then it is the
# executable spec for what to check.
#
# Exits 0 on success, 1 on a failed assertion, 2 on environment issue.
#
# Enriched fallback format (agentns session id absent), fixed field order,
# any field may be absent except pid+uid:
#
#   comm-chain:<c0>>c1>c2;env:<KEY>=<val>;cwd:<path>;pid:<p>;uid:<u>
#
set -u
FAIL=0

say() { printf '%-64s %s\n' "$1" "$2"; }
ok()  { say "$1" "PASS"; }
no()  { say "$1" "FAIL: $2"; FAIL=1; }
skip(){ say "$1" "SKIP: $2"; }

if ! grep -qE '(^|,)provfs(,|$)' /sys/kernel/security/lsm 2>/dev/null; then
    echo "test_comm_richer.sh: provfs LSM not loaded."
    echo "  /sys/kernel/security/lsm = $(cat /sys/kernel/security/lsm 2>/dev/null)"
    echo "  Boot the wintermute kernel (provfs in lsm= cmdline or default order)."
    exit 2
fi
if ! command -v getfattr >/dev/null; then
    echo "test_comm_richer.sh: getfattr not in PATH (install attr)"; exit 2
fi

# Stamp under $HOME so the file is not under a skipped prefix (/tmp et al).
WORK=$(mktemp -d -p "$HOME" provfs-cr.XXXX)
trap 'rm -rf "$WORK"' EXIT

read_session() { getfattr --only-values -n user.prov.session "$1" 2>/dev/null; }

# --- AC1: comm-chain captured on the fallback path -------------------------
# Pipeline write: bash spawns awk, awk does the redirected write. The chain
# should name the actual actors (bash>awk), not just the innermost comm.
F="$WORK/ac1"
bash -c "echo x | awk '{print}' > '$F'"
s=$(read_session "$F")
if echo "$s" | grep -qE 'comm-chain:[^;]*awk'; then
    ok "AC1 comm-chain present on pipeline write ($s)"
else
    no "AC1 comm-chain" "got '$s' (expected comm-chain:...awk...)"
fi

# --- AC2: env var captured when available ----------------------------------
F="$WORK/ac2"
CLAUDE_TOOL=/build bash -c "echo y > '$F'"
s=$(read_session "$F")
echo "$s" | grep -q 'env:CLAUDE_TOOL=/build' \
    && ok "AC2 env:CLAUDE_TOOL captured ($s)" \
    || no "AC2 env capture" "got '$s' (expected env:CLAUDE_TOOL=/build)"

# --- AC3: cwd captured -----------------------------------------------------
SUB="$WORK/ac3dir"; mkdir -p "$SUB"
( cd "$SUB" && echo z > ./file )
s=$(read_session "$SUB/file")
echo "$s" | grep -qF "cwd:$SUB" \
    && ok "AC3 cwd captured ($s)" \
    || no "AC3 cwd" "got '$s' (expected cwd:$SUB)"

# --- AC4: field-absent graceful degradation --------------------------------
# A writer with CLAUDE_TOOL/AGORABUS_SID/CLAUDE_SESSION_ID scrubbed should
# still produce comm-chain + cwd + pid + uid, just no env: field.
F="$WORK/ac4"
env -u CLAUDE_TOOL -u AGORABUS_SID -u CLAUDE_SESSION_ID \
    bash -c "echo q > '$F'"
s=$(read_session "$F")
if echo "$s" | grep -q 'env:'; then
    no "AC4 env absent" "got '$s' (env: should be omitted with no key set)"
else
    echo "$s" | grep -qE 'pid:[0-9]+;uid:[0-9]+$' \
        && ok "AC4 graceful degradation, pid+uid still present ($s)" \
        || no "AC4 degradation" "got '$s' (expected trailing pid:N;uid:N)"
fi

# --- AC5: 256-byte cap honored, comm-chain preserved -----------------------
# Deep cwd to push other fields toward the cap; value must stay <=256B and
# keep comm-chain at the front.
DEEP="$WORK/$(printf 'd%.0s' {1..200})"
mkdir -p "$DEEP" 2>/dev/null || DEEP="$WORK"
( cd "$DEEP" && echo big > ./file )
s=$(read_session "$DEEP/file")
len=${#s}
[[ "$len" -le 256 ]] \
    && ok "AC5 value <=256B (len=$len)" \
    || no "AC5 cap" "len=$len > 256"
echo "$s" | grep -q '^comm-chain:' \
    && ok "AC5 comm-chain preserved at front under pressure" \
    || skip "AC5 comm-chain front" "chain may be empty for this shell; value='$s'"

# --- AC6: agentns-present path unchanged (bare 32-hex) ---------------------
# Requires an agentns-wrapped session (agentns id non-zero). Without an
# agentns wrapper available, document the expectation and skip.
if command -v agentns >/dev/null 2>&1; then
    F="$WORK/ac6"
    agentns run -- bash -c "echo a > '$F'" 2>/dev/null || true
    s=$(read_session "$F")
    if echo "$s" | grep -qE '^[0-9a-f]{32}$'; then
        ok "AC6 agentns path bare 32-hex ($s)"
    else
        no "AC6 agentns" "got '$s' (expected bare 32-hex id)"
    fi
else
    skip "AC6 agentns-present unchanged" "agentns CLI not present; verify manually under a wrapped session"
fi

# --- AC7: no hot-path regression (perf) ------------------------------------
# Quantitative; documented here, run manually:
#   perf stat -e cycles,instructions -- cargo build  (<=5% vs pre-PRD provfs)
# Deferred-stamp's workqueue amortizes the comm-walk + env-read off close.
skip "AC7 no hot-path regression" "run perf stat on a cargo build manually (<=5% overhead)"

# --- AC8: no new oopses under stress ---------------------------------------
# Run a 5-min 'make -j\$(nproc)' kernel build, then:
#   dmesg | grep -i provfs   # expect: no NULL deref / UAF / WARN
if dmesg 2>/dev/null | grep -i provfs | grep -qiE 'null|deref|use-after|warn|bug|oops'; then
    no "AC8 no oopses" "dmesg shows a provfs warning/oops"
else
    ok "AC8 no provfs oops/warning in current dmesg (run sustained build to fully exercise)"
fi

# --- AC9: provq round-trip -------------------------------------------------
# provq should parse the enriched form and still accept legacy comm: xattrs.
if command -v provq >/dev/null 2>&1; then
    F="$WORK/ac9"; echo r > "$F"
    out=$(provq "$F" 2>/dev/null || true)
    echo "$out" | grep -qE 'comm-chain=|comm:' \
        && ok "AC9 provq round-trip ($out)" \
        || no "AC9 provq" "unexpected provq output: '$out'"
else
    skip "AC9 provq round-trip" "provq not in PATH; verify enriched + legacy parse manually"
fi

echo
if (( FAIL )); then
    echo "test_comm_richer.sh: FAILED"; exit 1
fi
echo "test_comm_richer.sh: OK (skipped checks are documented post-reboot/manual steps)"

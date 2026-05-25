#!/usr/bin/env bash
# Basic provfs LSM functional test. Runs after booting the wintermute kernel
# (the LSM is built-in, not modular).
#
# Exits 0 on success, 1 on any failed assertion, 2 on environment issue.
set -u
FAIL=0

say() { printf '%-60s %s\n' "$1" "$2"; }
ok()  { say "$1" "PASS"; }
no()  { say "$1" "FAIL: $2"; FAIL=1; }

if ! grep -qE '(^|,)provfs(,|$)' /sys/kernel/security/lsm 2>/dev/null; then
    echo "test_basic.sh: provfs LSM not loaded. /sys/kernel/security/lsm = $(cat /sys/kernel/security/lsm 2>/dev/null)"
    echo "Boot the wintermute kernel and ensure provfs is in the lsm= cmdline or default order."
    exit 2
fi
if ! command -v getfattr >/dev/null; then
    echo "test_basic.sh: getfattr not in PATH (install attr)"; exit 2
fi

TMPDIR=$(mktemp -d -p "$HOME" provfs-test.XXXX)
trap 'rm -rf "$TMPDIR"' EXIT
F="$TMPDIR/hello.txt"

# 1. write a file -> session + ts xattrs appear
echo hello-provfs > "$F"
session=$(getfattr --only-values -n user.prov.session "$F" 2>/dev/null)
ts=$(getfattr --only-values -n user.prov.ts "$F" 2>/dev/null)
[[ -n "$session" ]] && ok "user.prov.session stamped" || no "user.prov.session" "missing"
[[ -n "$ts"      ]] && ok "user.prov.ts stamped"      || no "user.prov.ts"      "missing"

# 2. session format is "comm:...:pid:N:uid:N"
echo "$session" | grep -qE '^comm:[^:]+:pid:[0-9]+:uid:[0-9]+$' \
    && ok "session value is comm:pid:uid form" \
    || no "session format" "got '$session'"

# 3. ts is unix seconds in the past minute
now=$(date +%s)
diff=$((now - ts))
[[ "$diff" -ge 0 && "$diff" -le 60 ]] && ok "ts within 60s of now" \
    || no "ts freshness" "diff=$diff (ts=$ts now=$now)"

# 4. /tmp is skipped
TMP_FILE=$(mktemp)
echo skip > "$TMP_FILE"
if getfattr -n user.prov.session "$TMP_FILE" 2>/dev/null | grep -q user.prov.session; then
    no "/tmp skipped" "session got stamped under /tmp"
else
    ok "/tmp skipped (no xattr)"
fi
rm -f "$TMP_FILE"

# 5. /proc is skipped — read-only anyway, just confirm getfattr says no xattr
got=$(getfattr -n user.prov.session /proc/self/status 2>&1 || true)
echo "$got" | grep -qi "no such attribute\|operation not supported" \
    && ok "/proc has no provfs xattrs" \
    || no "/proc skipped" "unexpected getfattr output: $got"

# 6. session encodes our pid
my_pid=$$
echo "$session" | grep -qE ":pid:${my_pid}(:|$)" \
    && ok "session pid matches \$\$ ($my_pid)" \
    || no "session pid" "got '$session', expected pid $my_pid"

# 7. session encodes our uid
my_uid=$(id -u)
echo "$session" | grep -qE ":uid:${my_uid}(:|$)" \
    && ok "session uid matches \$(id -u) ($my_uid)" \
    || no "session uid" "got '$session', expected uid $my_uid"

if (( FAIL )); then
    echo; echo "test_basic.sh: FAILED"; exit 1
fi
echo
echo "test_basic.sh: OK"

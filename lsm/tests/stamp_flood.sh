#!/usr/bin/env bash
# AC7 / AC8 — overflow drop + sysctl tune.
#
# AC7: flood the queue with many concurrent writes; expect queue_dropped
#      to move (at the default bound it may or may not, depending on
#      drain rate — lower queue_max via AC8 to force it), no oops, and
#      queue_depth to return to 0 within a few seconds of the flood end.
#
# AC8: `echo 64 | sudo tee /proc/sys/kernel/provfs/queue_max` then rerun;
#      drops should increase relative to the default.
#
# Requires a kernel booted with provfs in the active LSM set.
set -euo pipefail

N="${1:-10000}"
TESTDIR="${TESTDIR:-$HOME/test/provfs-flood}"
DROPPED=/proc/sys/kernel/provfs/queue_dropped
DEPTH=/proc/sys/kernel/provfs/queue_depth

mkdir -p "$TESTDIR"

read_counter() { [ -r "$1" ] && cat "$1" || echo "n/a"; }

before="$(read_counter "$DROPPED")"
echo "provfs-test: stamp_flood start N=$N queue_dropped(before)=$before"
if [ -r /proc/sys/kernel/provfs/queue_max ]; then
	echo "provfs-test: queue_max=$(cat /proc/sys/kernel/provfs/queue_max)"
fi

i=0
while [ "$i" -lt "$N" ]; do
	( printf 'flood-%d\n' "$i" > "$TESTDIR/f.$$.$i" ) &
	i=$((i + 1))
	# bound the number of live shells so we don't fork-bomb userspace;
	# the kernel queue is what we're trying to overflow.
	if [ $((i % 256)) -eq 0 ]; then wait; fi
done
wait

after="$(read_counter "$DROPPED")"
echo "provfs-test: queue_dropped(after)=$after"

# Wait for the queue to drain.
for _ in $(seq 1 10); do
	d="$(read_counter "$DEPTH")"
	[ "$d" = "0" ] && break
	sleep 0.5
done
echo "provfs-test: queue_depth(after drain)=$(read_counter "$DEPTH")"

if dmesg 2>/dev/null | tail -n 200 | grep -E 'BUG:|Oops:' ; then
	echo "FAIL: kernel splat during flood"
	exit 1
fi

if [ -r "$DEPTH" ] && [ "$(cat "$DEPTH")" != "0" ]; then
	echo "FAIL: queue_depth did not return to 0 (leak?)"
	exit 1
fi

echo "PASS: flood completed, depth drained, no splat (dropped: $before -> $after)"

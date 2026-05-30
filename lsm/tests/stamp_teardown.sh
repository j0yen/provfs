#!/usr/bin/env bash
# AC6 — teardown safety.
#
# Forks N children that each write+close+exit immediately. The point is
# to drive file_release from exit_files() / task teardown as hard as
# possible and confirm the deferred-stamp path never oopses there.
#
# Requires a kernel booted with provfs in the active LSM set. Run as a
# user that can write under $TESTDIR. Inspect dmesg afterward.
set -euo pipefail

N="${1:-1000}"
TESTDIR="${TESTDIR:-$HOME/test/provfs-teardown}"

mkdir -p "$TESTDIR"

dmesg_mark="$(date '+%s%N')"
echo "provfs-test: stamp_teardown start mark=$dmesg_mark N=$N dir=$TESTDIR"

i=0
while [ "$i" -lt "$N" ]; do
	(
		f="$TESTDIR/child.$$.$i"
		# write + close, then the subshell exits immediately, so the
		# final fput runs in exit_files() context.
		printf 'teardown-%d\n' "$i" > "$f"
	) &
	i=$((i + 1))
done
wait

# Give the workqueue a moment to drain.
sleep 2

echo "provfs-test: scanning dmesg for BUG/Oops since this run..."
if dmesg --since "@${dmesg_mark%???????}" 2>/dev/null | grep -E 'BUG:|Oops:|provfs.*WARN' ; then
	echo "FAIL: kernel splat detected after teardown flood"
	exit 1
fi

if [ -r /proc/sys/kernel/provfs/queue_depth ]; then
	depth="$(cat /proc/sys/kernel/provfs/queue_depth)"
	echo "provfs-test: queue_depth=$depth (expect 0 after drain)"
fi
if [ -r /proc/sys/kernel/provfs/queue_dropped ]; then
	echo "provfs-test: queue_dropped=$(cat /proc/sys/kernel/provfs/queue_dropped)"
fi

echo "PASS: no kernel splat after $N teardown writes"

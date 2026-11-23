#!/system/bin/sh
# Runs once after boot completes. Kernel interfaces are sampled read-only and the
# resulting capability document is saved in this module's data directory.

MODDIR=${0%/*}
DATA=/data/adb/neurosched
BIN="$MODDIR/bin/neuroschedd"
REPORT="$DATA/logs/capability.json"

[ -x "$BIN" ] || exit 0
mkdir -p "$DATA/logs" || exit 1

sleep 20

[ -f "$MODDIR/disable" ] && exit 0
[ -f "$MODDIR/remove" ] && exit 0
[ -f "$DATA/stop" ] && exit 0

if ! "$BIN" --data-dir "$DATA" --self-test >"$REPORT.tmp" 2>"$DATA/logs/self-test.err"; then
  echo "Capability check failed. Previous report retained." >&2
  exit 1
fi

# Keep the previous report for comparison across boots.
if [ -f "$DATA/logs/capability.prev.json" ]; then
  mv -f "$DATA/logs/capability.prev.json" "$DATA/logs/capability.prev2.json" || exit 1
fi
[ ! -f "$REPORT" ] || mv -f "$REPORT" "$DATA/logs/capability.prev.json" || exit 1
mv -f "$REPORT.tmp" "$REPORT" || exit 1

exit 0

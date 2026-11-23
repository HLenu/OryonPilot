#!/system/bin/sh
# KernelSU action button: print a compact status summary.
#
# Read-only. This never changes the mode and never writes to a kernel node.

MODDIR=${0%/*}
DATA=/data/adb/neurosched
BIN="$MODDIR/bin/neuroschedd"

if [ ! -x "$BIN" ]; then
  echo "neuroschedd not found"
  exit 1
fi

echo "== OryonPilot =="
echo "module: $(grep '^version=' "$MODDIR/module.prop" 2>/dev/null | cut -d= -f2)"
echo "data:   $DATA"
echo ""

pid=$(head -n 1 "$DATA/daemon.pid" 2>/dev/null)
case "$pid" in
  '' | *[!0-9]*) echo "daemon: not running" ;;
  *)
    if [ -d "/proc/$pid" ]; then
      expected=$(readlink -f "$BIN")
      actual=$(readlink "/proc/$pid/exe")
      if [ -n "$expected" ] && [ "$actual" = "$expected" ]; then
        echo "daemon: running (verified pid $pid)"
      else
        echo "daemon: PID identity unverified"
      fi
    else
      echo "daemon: pid file stale"
    fi
    ;;
esac
echo ""

echo "Read-only kernel snapshot. Live runtime state is available in the authenticated WebUI."
"$BIN" --data-dir "$DATA" --status 2>/dev/null | sed -n \
  -e 's/^  "mode": \(.*\)/mode:    \1/p' \
  -e 's/^    "state": \(.*\)/state:   \1/p' \
  -e 's/^    "reason": \(.*\)/reason:  \1/p'

echo ""
echo "webui: http://127.0.0.1:8787/"
echo "token: $DATA/token"

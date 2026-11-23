#!/system/bin/sh
# NeuroSched late_start supervisor. The daemon enforces its own single-instance lock.
# Stop markers persist until explicitly removed by the owner.

MODDIR=${0%/*}
DATA=/data/adb/neurosched
BIN="$MODDIR/bin/neuroschedd"
OUT="$DATA/logs/daemon.out"

[ -x "$BIN" ] || exit 1

stopping() {
  [ -f "$MODDIR/disable" ] || [ -f "$MODDIR/remove" ] || [ -f "$DATA/stop" ]
}

stopping && exit 0
mkdir -p "$DATA/logs" || exit 1
chmod 700 "$DATA" "$DATA/logs" || exit 1
: >>"$OUT" || exit 1

# A live PID is only recognized when its executable is this module's binary.
pid=$(head -n 1 "$DATA/daemon.pid" 2>/dev/null)
case "$pid" in
  '' | *[!0-9]*) ;;
  *)
    if [ -d "/proc/$pid" ]; then
      expected=$(readlink -f "$BIN")
      actual=$(readlink "/proc/$pid/exe")
      [ -n "$expected" ] && [ "$actual" = "$expected" ] && exit 0
      echo "daemon.pid belongs to an unverified process. Startup stopped." >>"$OUT"
      exit 1
    fi
    ;;
esac

# The boot wait runs in the background so other late_start scripts can continue.
(
  i=0
  while [ "$(getprop sys.boot_completed 2>/dev/null)" != "1" ]; do
    stopping && exit 0
    i=$((i + 1))
    [ "$i" -ge 180 ] && exit 1
    sleep 1
  done
  sleep 8

  backoff=5
  while :; do
    stopping && break
    # The previous child has exited, so its stdout file can be rotated safely.
    size=$(wc -c <"$OUT" 2>/dev/null)
    case "$size" in '' | *[!0-9]*) size=0 ;; esac
    if [ "$size" -gt 1048576 ]; then
      tail -c 524288 "$OUT" >"$OUT.tmp" && mv -f "$OUT.tmp" "$OUT" || exit 1
    fi
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] starting neuroschedd" >>"$OUT"
    "$BIN" --data-dir "$DATA" >>"$OUT" 2>&1 &
    child=$!
    while kill -0 "$child" 2>/dev/null; do
      if stopping; then
        # Signal only through the daemon's cooperative protocol. No PID is killed.
        "$BIN" --data-dir "$DATA" --stop >>"$OUT" 2>&1 || {
          echo "cooperative stop failed. State retained for inspection." >>"$OUT"
          exit 1
        }
        break
      fi
      sleep 2
    done
    wait "$child"
    code=$?
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] neuroschedd exited code=$code" >>"$OUT"
    stopping && break
    sleep "$backoff"
    backoff=$((backoff * 2))
    [ "$backoff" -gt 60 ] && backoff=60
  done
) &

exit 0

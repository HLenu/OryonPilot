#!/system/bin/sh
# NeuroSched uninstall. Preserve configuration, history and recovery evidence.
# Restoration requires verified daemon identity and confirmed process exit.

MODDIR=${0%/*}
DATA=/data/adb/neurosched
BIN="$MODDIR/bin/neuroschedd"

[ -d "$DATA" ] || exit 0
: >"$DATA/stop" || {
  echo "Could not create stop marker. Data retained at $DATA."
  exit 1
}

pid=$(head -n 1 "$DATA/daemon.pid" 2>/dev/null)
case "$pid" in
  '' | *[!0-9]*)
    if [ -f "$DATA/journal/active" ]; then
      echo "Active recovery journal has no verifiable daemon PID. Data retained at $DATA."
      exit 1
    fi
    echo "No active daemon session. Data retained at $DATA."
    exit 0
    ;;
esac

if [ ! -d "/proc/$pid" ]; then
  if [ -f "$DATA/journal/active" ]; then
    echo "Daemon PID is stale. Recovery needs manual inspection. Data retained at $DATA."
    exit 1
  fi
  echo "Daemon has exited. Data retained at $DATA."
  exit 0
fi

[ -x "$BIN" ] || {
  echo "Daemon binary is unavailable. Data retained at $DATA."
  exit 1
}
expected=$(readlink -f "$BIN")
actual=$(readlink "/proc/$pid/exe")
if [ -z "$expected" ] || [ "$actual" != "$expected" ]; then
  echo "PID $pid has an unverified executable. Data retained at $DATA."
  exit 1
fi

"$BIN" --data-dir "$DATA" --stop || {
  echo "Cooperative stop failed. Data retained at $DATA."
  exit 1
}
i=0
while [ -d "/proc/$pid" ] && [ "$i" -lt 30 ]; do
  actual=$(readlink "/proc/$pid/exe")
  if [ "$actual" != "$expected" ]; then
    echo "PID identity changed while stopping. Data retained at $DATA."
    exit 1
  fi
  sleep 1
  i=$((i + 1))
done
if [ -d "/proc/$pid" ]; then
  echo "Daemon did not exit within 30 seconds. Data retained at $DATA."
  exit 1
fi

"$BIN" --data-dir "$DATA" --restore-all || {
  echo "Recovery failed. Journal and data retained at $DATA."
  exit 1
}

echo "Daemon stopped and recovery command completed. Data retained at $DATA."
exit 0

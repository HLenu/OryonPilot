#!/usr/bin/env bash
# NeuroSched read-only device audit.
#
# Collects everything the daemon needs to know about a *specific* build, without
# changing a single value. Run it with the phone connected over ADB:
#
#   bash scripts/audit.sh                # writes docs/audit-<timestamp>.txt
#   bash scripts/audit.sh --stdout       # print instead
#
# READ-ONLY GUARANTEE
#   Every command sent to the device is one of: getprop, cat, ls, dumpsys, ps, uname,
#   id, grep, head, tail, wc, logcat -d, sed (read), tr, awk, find, stat, printf.
#   There is no write, no chmod, no property change, no module install, no reboot and
#   no partition operation. `--verify-readonly` re-checks this file mechanically.

set -uo pipefail

OUT_DIR="$(cd "$(dirname "$0")/.." && pwd)/docs"
STAMP="$(date +%Y%m%d-%H%M%S)"
TO_STDOUT=0
VERIFY=0
case "${1:-}" in
  --stdout) TO_STDOUT=1 ;;
  --verify-readonly) VERIFY=1 ;;
esac

# Mechanical self-check. Patterns are assembled from fragments so this function does
# not match itself.
verify_readonly() {
  fail=0
  check() {
    label="$1"; pat="$2"
    # Skip comments and the check() invocations themselves, which necessarily
    # mention the pattern they look for.
    hits=$(grep -n -- "$pat" "$0" | grep -v -e '^[0-9]*: *#' -e 'check "' || true)
    if [ -n "$hits" ]; then
      echo "FAIL  $label"
      echo "$hits" | sed 's/^/        /'
      fail=1
    else
      echo "ok    $label"
    fi
  }
  echo "read-only self-check: $0"
  check "no property writes"      "set""prop"
  check "no reboot"               "re""boot"
  check "no fastboot"             "fast""boot"
  check "no partition flash"      "fla""sh "
  check "no filesystem creation"  "mk""fs"
  check "no raw disk writes"      "dd ""if="
  check "no delete"               "rm ""-"
  check "no move/rename"          "mv ""/"
  check "no permission change"    "ch""mod"
  check "no remount"              "mount ""-o"
  check "no kernel module load"   "ins""mod"
  check "no package install"      "pm ""install"
  check "no settings writes"      "settings ""put"
  check "no power service calls"  "svc ""power"
  check "no swapon/swapoff"       "swap""on"
  if [ "$fail" -ne 0 ]; then
    echo
    echo "RESULT: NOT read-only"
    return 1
  fi
  echo
  echo "RESULT: read-only (no device-mutating command found)"
  return 0
}

if [ "$VERIFY" = "1" ]; then
  verify_readonly
  exit $?
fi

if ! command -v adb >/dev/null 2>&1; then
  echo "adb not found in PATH" >&2
  exit 1
fi

DEVICES="$(adb devices | awk 'NR>1 && $2=="device" {print $1}')"
if [ -z "$DEVICES" ]; then
  echo "no device connected (adb devices is empty)" >&2
  echo "this script performs a real-device audit only; nothing was simulated." >&2
  exit 2
fi
SERIAL="${ANDROID_SERIAL:-}"
if [ -z "$SERIAL" ]; then
  if [ "$(printf '%s\n' "$DEVICES" | wc -l)" -ne 1 ]; then
    echo "multiple devices connected. Set ANDROID_SERIAL to the intended target." >&2
    exit 2
  fi
  SERIAL="$DEVICES"
fi
case "$SERIAL" in
  *[!a-zA-Z0-9._:-]*) echo "invalid ANDROID_SERIAL" >&2; exit 2 ;;
esac
if ! printf '%s\n' "$DEVICES" | grep -Fx -- "$SERIAL" >/dev/null; then
  echo "selected device is unavailable: $SERIAL" >&2
  exit 2
fi

ADB="adb -s $SERIAL"
SU=""
if $ADB shell "command -v su >/dev/null 2>&1 && echo yes" 2>/dev/null | grep -q yes; then
  SU="su -c"
fi

# sh <command>: run as root when possible, otherwise as shell.
sh_() {
  if [ -n "$SU" ]; then
    $ADB shell "$SU \"$1\"" 2>/dev/null
  else
    $ADB shell "$1" 2>/dev/null
  fi
}

section() { printf '\n===== %s =====\n' "$1"; }

collect() {
  echo "NeuroSched device audit"
  echo "serial: $SERIAL"
  echo "host date: $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  echo "root: ${SU:-no}"

  section "1. identity"
  $ADB shell getprop 2>/dev/null | grep -E '^\[(ro\.product\.(device|model|name)|ro\.build\.(fingerprint|version\.release|version\.sdk|date)|ro\.board\.platform|ro\.soc\.(model|manufacturer)|ro\.product\.cpu\.abi|ro\.boot\.(flash|hardware)|init\.svc\.mmd|persist\.sys\.(mmd|zram).*)\]' | sort

  section "2. kernel / root"
  sh_ "uname -a"
  sh_ "cat /proc/version"
  sh_ "id"
  sh_ "getenforce 2>/dev/null || cat /sys/fs/selinux/enforce"
  sh_ "cat /data/adb/ksu/version 2>/dev/null; ksud -V 2>/dev/null; cat /data/adb/magisk/util_functions.sh 2>/dev/null | head -3"
  sh_ "ls -la /data/adb/modules 2>/dev/null"

  section "3. page size / abi"
  sh_ "grep -E 'KernelPageSize|MMUPageSize' /proc/self/smaps 2>/dev/null | head -2"
  sh_ "getprop ro.product.cpu.abilist"

  section "4. cpu topology"
  for f in present possible online; do
    printf '%-10s %s\n' "$f" "$(sh_ "cat /sys/devices/system/cpu/$f")"
  done
  sh_ "ls -d /sys/devices/system/cpu/cpufreq/policy* 2>/dev/null"
  for p in $(sh_ "ls -d /sys/devices/system/cpu/cpufreq/policy* 2>/dev/null"); do
    echo "--- $p"
    for k in related_cpus affected_cpus scaling_driver scaling_governor \
             scaling_available_governors scaling_min_freq scaling_max_freq \
             scaling_cur_freq cpuinfo_min_freq cpuinfo_max_freq; do
      v="$(sh_ "cat $p/$k")"
      [ -n "$v" ] && printf '  %-28s %s\n' "$k" "$v"
    done
  done
  sh_ "for c in /sys/devices/system/cpu/cpu[0-9]*; do echo -n \"\$(basename \$c) cap=\"; cat \$c/cpu_capacity 2>/dev/null || echo -; done"
  sh_ "for c in /sys/devices/system/cpu/cpu[0-9]*; do echo -n \"\$(basename \$c) max=\"; cat \$c/cpufreq/cpuinfo_max_freq 2>/dev/null || echo -; done"

  section "5. scheduler interfaces"
  sh_ "cat /proc/self/mountinfo"
  for n in /proc/sys/kernel/sched_util_clamp_min \
           /proc/sys/kernel/sched_util_clamp_max \
           /proc/sys/kernel/sched_walt_rotate_window \
           /proc/sys/kernel/sched_autogroup_enabled \
           /sys/fs/cgroup/cgroup.controllers \
           /sys/fs/cgroup/top-app/cpu.uclamp.min \
           /sys/fs/cgroup/top-app/cpu.uclamp.max \
           /dev/cpuctl/top-app/cpu.uclamp.min \
           /dev/cpuctl/top-app/cpu.uclamp.max \
           /sys/fs/cgroup/cpu/top-app/cpu.uclamp.min \
           /sys/fs/cgroup/cpu/top-app/cpu.uclamp.max \
           /dev/stune/top-app/schedtune.boost; do
    v="$(sh_ "cat $n 2>/dev/null")"
    printf '%-52s %s\n' "$n" "${v:-<missing>}"
    sh_ "ls -ln $n 2>/dev/null"
  done
  sh_ "ls /sys/fs/cgroup 2>/dev/null | head -30"
  sh_ "ls /dev/cpuctl 2>/dev/null"
  sh_ "cat /proc/cgroups 2>/dev/null"

  section "6. memory"
  sh_ "cat /proc/meminfo | head -20"
  sh_ "cat /proc/swaps"
  sh_ "grep -E '^(pswpin|pswpout|pgscan_kswapd|pgsteal_kswapd|pgmajfault|pgscan_direct)' /proc/vmstat"
  sh_ "cat /sys/block/zram0/mm_stat 2>/dev/null; echo; cat /sys/block/zram0/disksize 2>/dev/null"
  sh_ "cat /proc/sys/vm/swappiness /proc/sys/vm/min_free_kbytes 2>/dev/null"

  section "7. psi"
  for p in cpu memory io; do
    echo "--- /proc/pressure/$p"
    sh_ "cat /proc/pressure/$p 2>/dev/null || echo '<missing>'"
  done

  section "8. mmd"
  sh_ "getprop init.svc.mmd"
  sh_ "ls -l /system/bin/mmd /system_ext/bin/mmd 2>/dev/null"
  sh_ "ps -A 2>/dev/null | grep -E 'mmd|kswapd' | head -10"
  sh_ "ls /data/vendor/mmd 2>/dev/null; ls /data/system/mmd 2>/dev/null"

  section "9. thermal"
  sh_ "dumpsys thermalservice 2>/dev/null | head -60"
  sh_ "for z in /sys/class/thermal/thermal_zone*; do echo -n \"\$z \"; cat \$z/type 2>/dev/null | tr -d '\n'; echo -n ' = '; cat \$z/temp 2>/dev/null; done"
  sh_ "ls /sys/class/thermal | head -40"

  section "10. power"
  for f in capacity temp status current_now voltage_now; do
    printf 'battery/%-12s %s\n' "$f" "$(sh_ "cat /sys/class/power_supply/battery/$f 2>/dev/null")"
  done
  sh_ "ls /sys/class/power_supply"

  section "11. modules and competing services"
  sh_ "ls -la /data/adb/modules/*/module.prop 2>/dev/null"
  sh_ "ps -A -o PID,USER,NAME 2>/dev/null | grep -Ei 'scene|tuner|thrawl|perf|boost|thermal|mmd|kswapd' | head -20"
  sh_ "getprop | grep -Ei 'scene|perf|boost' | head -20"
  sh_ "ls -la /data/adb/scene 2>/dev/null | head -20"

  section "12. app events capability"
  sh_ "ls -l /system/bin/logcat"
  sh_ "logcat -b events -d -t 5 2>/dev/null | tail -5"

  section "13. done"
  echo "audit complete"
}

if [ "$TO_STDOUT" = "1" ]; then
  collect
else
  mkdir -p "$OUT_DIR"
  FILE="$OUT_DIR/audit-$STAMP.txt"
  collect | tee "$FILE"
  echo
  echo "written: $FILE"
fi

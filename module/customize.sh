#!/system/bin/sh
# NeuroSched installer (KernelSU / Magisk compatible)
#
# Installation creates module data and checks binary executability.

SKIPUNZIP=0

# Magisk exports MODVER; some KernelSU builds do not. Read it from module.prop instead
# of printing an empty version banner.
if [ -z "${MODVER:-}" ] && [ -f "$MODPATH/module.prop" ]; then
  MODVER=$(sed -n 's/^version=//p' "$MODPATH/module.prop" | head -n 1)
fi

ui_print " "
ui_print "======================================"
ui_print "  OryonPilot  $MODVER"
ui_print "  device: $ARCH / API $API"
ui_print "======================================"
ui_print " "

if [ "$ARCH" != "arm64" ]; then
  ui_print "! 仅支持 arm64，当前为 $ARCH"
  abort "! 架构不匹配"
fi

DATA=/data/adb/neurosched
BIN="$MODPATH/bin/neuroschedd"

if [ ! -f "$BIN" ]; then
  ui_print "! 模块包内缺少 bin/neuroschedd"
  abort "! 安装包不完整"
fi

mkdir -p "$DATA/logs" "$DATA/telemetry" "$DATA/journal" "$DATA/backups"
chmod 700 "$DATA"
chmod 700 "$DATA/logs" "$DATA/telemetry" "$DATA/journal" "$DATA/backups"

# Default configuration is only written when absent: an upgrade must never
# overwrite tuned thresholds.
if [ ! -f "$DATA/config.json" ]; then
  if [ -f "$MODPATH/config.default.json" ]; then
    cp -f "$MODPATH/config.default.json" "$DATA/config.json"
    chmod 600 "$DATA/config.json"
    ui_print "- 已写入默认配置（observe_only）"
  else
    ui_print "- 未内置默认配置，首次启动时由守护进程生成"
  fi
else
  ui_print "- 保留已有配置"
fi

set_perm_recursive "$MODPATH" 0 0 0755 0644
set_perm "$MODPATH/bin/neuroschedd" 0 0 0755
set_perm "$MODPATH/service.sh" 0 0 0755
set_perm "$MODPATH/boot-completed.sh" 0 0 0755
set_perm "$MODPATH/uninstall.sh" 0 0 0755
set_perm "$MODPATH/action.sh" 0 0 0755
set_perm "$MODPATH/customize.sh" 0 0 0755

# Verify the binary actually runs on this device before declaring success.
if "$BIN" --version >/dev/null 2>&1; then
  ui_print "- 守护进程可执行: $("$BIN" --version 2>/dev/null)"
else
  abort "! 守护进程无法执行，安装中止。请核实静态 aarch64 二进制兼容性。"
fi

ui_print "- 数据目录: $DATA"
ui_print "- 默认配置: observe_only，ACTUATOR_ENABLED=false"
ui_print "- 已有配置继续保留，实际状态见鉴权 WebUI"
ui_print "- WebUI 见模块卡片或 http://127.0.0.1:8787/"
ui_print " "

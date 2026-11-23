# OryonPilot 使用手册

2026-11-07 当前为只读观察测试版：289项Rust回归通过，新版在目标小米15完成自检和短时隔离运行。现有 `neurosched` 兼容标识继续使用。安全默认值为 `mode="observe_only"`、`actuator.enabled=false`、`ACTUATOR_ENABLED=false`。安装、验证、遗留问题与回滚见 [本轮交付](docs/OPTIMIZATION_2026-11-07.md)。长期稳定性和收益仍待验收。

适用设备：小米 15 / 骁龙 8 Elite / HyperOS 4（Android 17）/ KernelSU / ARM64
版本：v0.1.1-observe.1（versionCode 102）

---

## 0. 先读这一段

OryonPilot 当前没有经过 A/B 测试证明的性能或续航提升数据。

它做三件事：

1. **看**：把 CPU 簇、PSI、zRAM、温度、前台应用按 1 Hz 采集下来，本地存 CSV，不联网。
2. **判**：用一套明确的状态机（9 个状态 + 优先级 + 滞回 + 冷却）决定"现在该是什么档位"，并给出理由。
3. **模拟**：生成策略及下一应用预览。当前构建硬性禁止实际内核写入；已有事务与回滚只在合成设备树验证。

它**不会**：改 CPU 频率、改温控阈值、改 zRAM/LMKD/swappiness、关 SELinux、刷分区、重启手机、从网页执行 shell。

**默认安装是 `observe_only`：只读，不写任何内核参数。** 这是刻意的。

> 实机状态：小米15 / Android17 / SukiSU Ultra 的短时只读验证通过。完整升级/卸载、WebView、24–48小时与A/B尚未验证，详见 `docs/DEVICE_CAPABILITY.md`。当前包无法从界面开启内核写入。

---

## 1. 安装

### 1.1 前置

- 已解锁 Bootloader 并刷入 KernelSU（或支持模块的 KSU 分支）
- 系统为 HyperOS 4 / Android 17，arm64
- 已安装 Scene（可选，共存策略见 7.3）

### 1.2 安装模块

```text
SukiSU Ultra → 模块 → 从本地安装 → 选择 neurosched-v0.1.1-observe.1-arm64.zip
```

安装时屏幕会打印：

```
- 已写入默认配置（observe_only）
- 守护进程可执行: neuroschedd 0.1.1-observe.1 (102)
- 数据目录: /data/adb/neurosched
- 当前模式: observe_only（不写入任何内核参数）
```

如果看到 `! 守护进程无法执行`，说明这个内核跑不了静态 aarch64 ELF（极少见），
模块不会启动，也不会影响系统。此时请保留日志并反馈。

### 1.3 验证安装

```bash
adb shell su -c "/data/adb/modules/neurosched/bin/neuroschedd --version"
adb shell su -c "cat /data/adb/neurosched/logs/neurosched.log | tail -20"
```

在你方便时从系统电源菜单正常重启，使升级后的模块生效。安装会保留已有配置和数据。通过版本命令确认 `0.1.1-observe.1 (102)`，再用鉴权 WebUI 检查模式为 `observe_only`、执行硬门关闭、采集时间持续更新。CLI `--status` 是新采样，不是常驻进程状态。

---

## 2. 打开控制台

三种方式，任选：

| 方式 | 操作 |
|---|---|
| 模块 WebUI | KernelSU 模块卡片 → 打开（内嵌 `127.0.0.1:8787`） |
| 手机浏览器 | 访问 `http://127.0.0.1:8787/` |
| 电脑浏览器（USB） | `adb forward tcp:8787 tcp:8787` 后访问 `http://127.0.0.1:8787/` |

控制台只监听回环地址，所有 `/api/*` 需要令牌。令牌在 `/data/adb/neurosched/token`，
由守护进程首次启动时生成（32 位十六进制，权限 0600）。持有者从设备本地读取后，在登录框手动输入。页面内存保存凭证，API 使用 Header 认证，刷新后重新输入。

---

## 3. 第一步：只读审计（强烈建议）

### 3.1 设备侧

```bash
adb shell su -c "/data/adb/modules/neurosched/bin/neuroschedd --self-test"
```

输出 JSON，重点看：

| 字段 | 含义 | 期望 |
|---|---|---|
| `cpu.clusters` | 识别到的频率簇数量 | 2 |
| `cpu.policies[].name` | 按能力排序的簇名 | `prime` / `gold` |
| `actuator.uclamp` | uclamp 节点与**尺度** | `percent_0_100` 或 `perf_0_1024` |
| `actuator.uclamp` 为 `null` | 该内核没有 cgroup uclamp | 只能监控，无法执行 |
| `memory.psi_memory` | PSI 是否可用 | `true` |
| `mmd.present` | MMD 二进制或进程是否存在 | 不等同正在运行；结合 pid 与 service 查看 |
| `thermal.mapping` | 每个温度 zone 被归为哪一类 | `cpu`/`gpu`/`skin`/`battery`/`other` |
| `failures` | 关键采集失败数 | 0 |

模块在每次开机后 20 s 会自动把这份报告写到 `/data/adb/neurosched/logs/capability.json`。

### 3.2 电脑侧（只读 shell 审计）

```bash
bash scripts/audit.sh            # 需要设备已连接并授权 ADB
# 输出 docs/audit-<时间戳>.txt
```

它执行的全是 `getprop` / `cat` / `ls` / `dumpsys` / `ps`，**没有任何写操作**。

### 3.3 看 Dashboard

正常状态下应该看到：

- 两个簇（`prime` 6/7 核、`gold` 0-5 核），频率随负载变化
- PSI、温度与当前负载相符，不用固定阈值判断适配成功
- 缺失或过期的温度应显示未知，不能拿电池温度冒充CPU温度
- 采集器健康全部 `ok`（`app_events` 显示 `source=logcat` 最理想）

---

## 4. 三种运行模式

| 模式 | 写入 | 用途 |
|---|---|---|
| `observe_only` | ❌ | 默认。长期监控，建立基线 |
| `dry_run` | ❌（只报告） | 看策略"想做什么"，逐节点显示 `old → new` |
| `enabled` | 当前构建关闭 | 仅保留兼容解析和合成测试，API 拒绝启用 |

切换方式：**Safety 页 → 运行模式**，或命令行：

```bash
adb shell su -c "/data/adb/modules/neurosched/bin/neuroschedd --data-dir /data/adb/neurosched --mode dry_run"
```

命令行覆盖仅作用于该进程。已有 Daemon 由内核锁保护，第二实例启动失败。WebUI 的只读模式切换会保存配置并请求热重载，自定义 `--config` 路径同样适用。

验收依次进行只读采集、策略模拟、实机接口与厂商共存检查。受控执行资格需要另行审查和明确批准。

---

## 5. 界面说明

### 5.1 总览

当前状态与理由、CPU 总负载、可用内存、温度、各簇频率/负载条、PSI/kswapd/swap 压力条、
历史趋势图（CPU%、可用内存、PSI some10、CPU 温度）、电池、采集器健康。新增 Daemon 自身 CPU、RSS、线程数、主线程上下文切换、采集耗时；CPU按单核100%计，上下文切换不是硬件唤醒次数。页面隐藏时暂停请求。可导出不含凭证和应用历史的脱敏诊断 JSON。

### 5.2 CPU

簇表格（CPU 列表、governor、当前/最小/最大/峰值频率、容量、负载）、每核心负载与频率、
调度器能力（WALT 是否存在、uclamp 是否可探测）、发现的节点清单（只读探测结果）。

### 5.3 内存

`MemAvailable`、swap、zRAM 压缩比、`kswapd0` CPU%、内存明细、PSI（mem some/full、cpu some、io some）、
zRAM 设备表、MMD 状态（只显示，不接管）。

### 5.4 预测

下一次前台应用的预测、概率、置信度、样本数、准确率、候选列表、使用排行。
**预测只影响档位准备，不会启动任何应用。** 样本不足或置信度低时会明确显示"已拒绝"。

### 5.5 策略

当前决策（状态、原因、已保持时长、热权限、内存分类、uclamp 目标值）、状态优先级列表（高亮当前）、
本次所有可用状态及其触发原因、状态迁移历史、阈值配置表单（保存后立即热重载）。

### 5.6 安全

运行模式切换、Watchdog 状态、SAFE_MODE 提示、写入统计、冲突、崩溃恢复记录、
uclamp 能力（含**尺度**）、节点所有权表（原始值 / 最后写入 / 是否已释放）、写入事务日志、回滚按钮。

### 5.7 日志

按级别过滤 + 关键字过滤，可导出 CSV。

---

## 6. 状态机

| 优先级 | 状态 | 何时进入 |
|---|---|---|
| 100 | `SAFE_MODE` | 连续启动失败达阈值，或手动 |
| 90 | `THERMAL_GUARD` | CPU 类温度 ≥ 50 °C（默认），降到 42 °C 以下才退出 |
| 80 | `MEMORY_PRESSURE` | PSI some>12 或 full>3 或可用内存<8% 或 kswapd>25% |
| 70 | `BATTERY_SAVER` | 电量 ≤ 15% 且未充电 |
| 60 | `APP_LAUNCH` | 前台变化后 2.5 s 内 |
| 50 | `GAME` | 前台包名在 `policy.game.packages` 里 |
| 40 | `FOREGROUND` | 有前台应用、非桌面、屏幕亮 |
| 30 | `BALANCED` | 兜底（无其它信号时） |
| 20 | `IDLE` | 屏幕关闭 |
| 10 | `BOOT` | 启动后 90 s 内 |

防抖：进入/退出阈值不同 + 最短驻留（默认 4 s）+ 冷却（默认 8 s）+ 保护态可立即抢占。
所以你会看到"温度已经降了但状态还停在 THERMAL_GUARD 几秒"——这是设计，不是卡住。

每个状态的 uclamp 目标值（0..1024，写入时按节点尺度自动换算）：

| 状态 | uclamp.min | uclamp.max |
|---|---|---|
| `APP_LAUNCH` | 320 | 1024 |
| `GAME` | 256 | 1024 |
| `FOREGROUND` | 128 | 1024 |
| `MEMORY_PRESSURE` | 0 | 768 |
| `THERMAL_GUARD` | 0 | 640 |
| `BATTERY_SAVER` | 0 | 512 |
| 其它 | 0 | 1024 |

温度越高，`uclamp.max` 上限还会被进一步压低（960 → 800 → 640），且任何 boost 都会被清零。

---

## 7. 常见问题

### 7.1 状态一直是 BOOT

启动后 90 s 内就是 `BOOT`，属正常。超过 90 s 仍是 `BOOT`，检查
`/data/adb/neurosched/logs/neurosched.log` 是否有采样错误。

### 7.2 温度显示 `—` / 热保护不触发

说明没有任何 thermal zone 被识别为 CPU 类。去 Safety 页看 `thermal.mapping`，
或运行 `--self-test` 看 `thermal.mapping`。若全是 `other`，说明 HyperOS 的命名规则不同，
此时实际执行资格保持关闭，策略按温度不可信处理；请导出脱敏诊断检查，不能假定设备温度正常。

### 7.3 和 Scene 冲突怎么办

当前包不写内核调度参数，因此不会与 Scene 争写该参数。已有所有权冲突检测只经合成测试；未来开启执行前仍需实机验证厂商服务的写入与协作方式，不能把回读比较当作共存已验证。

**不要**为了"让两个都生效"去关掉检测。也不要卸载 Scene。

### 7.4 `enabled` 模式下写入全是 `failed`

当前构建的 `ACTUATOR_ENABLED=false`，生产根的执行与恢复均拒绝写入。历史事务错误分类保留如下，供合成测试诊断。

看 Safety 页的"写入事务日志"里的 `result`：

| result | 含义 | 处理 |
|---|---|---|
| `write_error(Permission denied)` | SELinux 或 cgroup 挂载限制 | 保持 `observe_only`，反馈日志 |
| `verify_failed(readback=…)` | 内核 clamp 或忽略了该值 | 多半是尺度判断错，反馈 `capability.json` |
| `conflict` | 有别的服务在写同一节点 | 正常降级，无需处理 |

### 7.5 预测一直是"已拒绝"

样本不足（默认需要 40 个事件）或置信度低于 0.35。正常使用一两天后会开始给出预测。
如果 `source=none`，说明 logcat 事件被裁剪且 `/proc` 扫描也没拿到——此时预测永远不会工作，
但监控与策略仍然正常。

### 7.6 怎么完全停掉

```bash
adb shell su -c "/data/adb/modules/neurosched/bin/neuroschedd --stop"
```

守护进程会在退出前把所有自己写过的节点恢复原值，然后停止（`service.sh` 不会自动重启它，
因为看到了 `stop` 标记）。该标记跨重启保留，由设备持有者在确认恢复状态后显式清除，并重新启动服务。

### 7.7 卸载

KernelSU 模块 → 卸载。`uninstall.sh` 会：停止守护进程 → 等待退出 → `--restore-all` → 删除 `/data/adb/neurosched`。

**不会**动 Scene、不会动其它模块、不会改系统分区。

如果卸载后仍怀疑有残留，手动检查：

```bash
adb shell su -c "cat /sys/fs/cgroup/top-app/cpu.uclamp.min /sys/fs/cgroup/top-app/cpu.uclamp.max"
```

默认值通常是 `0` 和 `100`（百分比尺度）或 `0` 和 `1024`（性能点尺度）。

---

## 8. 配置参考

配置文件：`/data/adb/neurosched/config.json`（权限 0600）。WebUI 保存时会做范围钳制，
手改后重启或点"重新载入"生效。

```jsonc
{
  "mode": "observe_only",              // observe_only | dry_run | enabled
  "sample": {
    "interval_ms": 1000,               // 正常采样间隔
    "screen_off_interval_ms": 5000,    // 息屏降频采样
    "thermal_interval_ms": 4000,       // 温度缓存时间
    "history_interval_ms": 60000,      // 历史聚合间隔
    "proc_scan_interval_ms": 5000      // /proc 前台扫描（仅兜底时用）
  },
  "api": { "enabled": true, "bind": "127.0.0.1", "port": 8787 },
  "policy": {
    "min_dwell_ms": 4000,
    "cooldown_ms": 8000,
    "boot_grace_ms": 90000,
    "thermal": {
      "enter_light_c": 45, "enter_moderate_c": 50,
      "enter_severe_c": 56, "enter_critical_c": 62,
      "exit_margin_c": 3, "include_skin": false
    },
    "memory": {
      "psi_some_enter": 12, "psi_some_exit": 4,
      "psi_full_enter": 3, "psi_full_exit": 0.5,
      "avail_pct_enter": 8, "avail_pct_exit": 14,
      "kswapd_pct_enter": 25, "kswapd_pct_exit": 8,
      "swap_in_rate_enter": 4096
    },
    "battery": { "saver_pct": 15, "exit_pct": 20, "ignore_while_charging": true },
    "app_launch": { "boost_ms": 2500 },
    "game": { "packages": [], "detect_by_gpu_load": false },
    "idle": { "screen_off_after_ms": 5000 }
  },
  "actuator": {
    "enabled": false,
    "enable_uclamp_top_app": true,
    "enable_sched_util_clamp": false,  // 全局，默认关闭
    "profiles": { "FOREGROUND": { "uclamp_min": 128, "uclamp_max": 1024 } /* ... */ }
  },
  "predictor": { "enabled": true, "min_samples": 40, "min_confidence": 0.35 },
  "safety": { "consecutive_failure_limit": 3, "safe_mode_boots": 3, "on_conflict": "release" }
}
```

**游戏包名**：把游戏包名填进 `policy.game.packages`（数组），例如
`["com.miHoYo.Yuanshen", "com.tencent.tmgp.sgame"]`。

---

## 9. 数据与隐私

| 位置 | 内容 |
|---|---|
| `/data/adb/neurosched/telemetry/history.csv` | 60 s 聚合的负载/内存/温度/前台包名 |
| `/data/adb/neurosched/logs/` | 运行日志、能力报告 |
| `/data/adb/neurosched/journal/` | 写入事务日志（含节点原始值） |
| `/data/adb/neurosched/model.json` | 应用切换统计（包名 + 计数） |

- 全部保存在本机，**不向任何服务器发送**，守护进程没有任何网络客户端代码。
- 包名属于敏感信息，导出 CSV 分享前请自行确认。
- 删除模块即删除全部数据。

---

## 10. 已知限制

1. **只写 top-app 的 uclamp**，不改频率表。因此不要期待"跑分变化"。
2. `uclamp.max` 受内核 rq 聚合语义限制：同 rq 上有未受限任务时封顶会失效。
3. Android 17 的 MMD 已接管 zRAM，本项目**不做** zRAM/swap/LMKD 调优。
4. 温度分类依赖 zone 的 `type` 命名，命名不规范时热保护不生效（UI 会显示）。
5. 前台识别首选 `logcat -b events`；被裁剪时降级为 `/proc` 扫描，预测置信度上限 50%。
6. WebUI 未在 KernelSU WebView 内实机验证；若 iframe 被限制，用浏览器直接访问。
7. 无 Kotlin Companion App 的预编译 APK（开发环境无 Android SDK），但其数据通道
   `fg_events.log` 已实现：向该文件追加一行包名即可被识别。

---

## 11. 出问题时请提供

```bash
adb shell su -c "cat /data/adb/neurosched/logs/neurosched.log" > neurosched.log
adb shell su -c "/data/adb/modules/neurosched/bin/neuroschedd --self-test" > capability.json
adb shell su -c "tail -50 /data/adb/neurosched/journal/*.jsonl" > journal.txt
```

加上机型、HyperOS 版本、内核版本，基本就能定位。

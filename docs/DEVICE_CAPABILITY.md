# 设备能力报告

## 2026-11-07 v0.1.1-observe.1 验证补充（当前）

新版在同一小米15（ADB 8d053a1f、dada、Root uid0）以临时目录运行，不替换旧模块。self-test/once exit0、自检0 failures；隔离Runtime约20秒、多帧鉴权诊断、正常停止和子进程清理通过。5个模块脚本在Android `/system/bin/sh -n`语法通过，没有执行安装脚本。**新包升级/卸载、WebView与长期使用仍未验收。** 旧版已由用户安装，首次开机fsck延迟调查见PROGRESS，不能套用下方历史“未安装”状态。

WALT探测已采用精确governor证据，实机输出true；页大小展示复用sysconf/auxv，实机输出4kB。MMD进程匹配限定argv[0]basename=mmd，二进制存在不等于运行。自身资源读数来自/proc，三帧RSS896–960KiB、16线程，CPU0.396–0.990%（单核口径），仅短时观察，不代表长期功耗。

boot_id绑定、HTTP限额和活性监控已接线；临时Runtime写日志0字节、生产执行硬门false、模式observe_only。正常停止约0.589秒，journal active清理，logcat子进程退出。检测脚本首次发现该内核没有`/proc/<pid>/task/<pid>/children`，改用只读PID/PPID表后通过；未凭经验假定该文件存在。临时文件和转发均已清理。

当前包 `neurosched-v0.1.1-observe.1-arm64.zip`，SHA256 `07746e2b10c64499b5294de622865eaec3d35541f7b7c258d5daece4c69d336d`。文件、证据路径、命令、风险及回滚以 [本轮优化交付](OPTIMIZATION_2026-11-07.md) 为准。以下251项/旧包及未安装声明属于此前时间点。

---

## 2026-11-07 小米15首次只读适配检查（历史）

**结论：ARM64只读程序已在目标小米15上运行通过，主要接口匹配；已修复3处解析问题。常驻服务、模块生命周期和日用验收仍为 PARTIAL。** 本次先通过ADB/su读取，再将已审查的程序临时放入`/data/local/tmp/oryonpilot-check.*`，仅执行`--self-test`和`--once`，随后删除临时文件并验证清理成功。没有安装模块、启动常驻服务、写内核参数或重启。

用户确认安装 SukiSU Ultra 并授权 Shell 后，读取身份为 `uid=0`、`u:r:ksu:s0`，SELinux 保持 Enforcing。授权前读失败不再构成当前阻塞。ADB 同时连接其他设备，本次始终用 `-s 8d053a1f` 选择小米15。

### 设备与接口实证

| 检查 | 实际读取 | 适配判断 |
|---|---|---|
| 设备 | Xiaomi 24129PN74C，dada，SM8750 / sun，arm64-v8a | 与目标硬件一致 |
| 系统 | Android17 / SDK37，OS4.0.0.22.XOCCNXM | 与目标系统一致 |
| 内核 / 页大小 | 6.6.118-android15-8-gb9cc6ec16bc8-abogki536571621-4k；aarch64，4096B | ARM64静态ELF已在此内核成功执行只读自检 |
| Root接口 | `4.2.1-2-gdfae9237:KernelSU`，40994 | SukiSU Shell Root只读访问成功；不等于模块安装兼容性已验收 |
| CPU拓扑 | policy0=CPU0–5，capacity792，最高3532800kHz；policy6=CPU6–7，capacity1024，最高4320000kHz；8核在线 | 现有拓扑代码正确解析为6+2，不硬编码核号 |
| CPU调频 | driver=scmi，两个policy的governor均为walt | 当次scaling_max_freq低于cpuinfo_max_freq，原因未确定，不归因温控或特定服务 |
| CPU cgroup | `/dev/cpuctl` 为cgroup v1，挂载含cpu控制器；同时存在cgroup2树 | 实际top-app接口走v1，不能因存在cgroup2就选择v2路径 |
| uclamp | `/dev/cpuctl/top-app/cpu.uclamp.min=0.00`，max=`max`；0644 root:root，cgroup SELinux标签 | 代码按cgroup百分比ABI读取0–100及max；挂载与原始读值验证通过；未做写探测 |
| 全局uclamp | `/proc/sys/kernel/sched_util_clamp_min/max` 均读到1024 | 与cgroup百分比是不同接口；不据此推断每个任务被强制满负载 |
| 内存 / PSI | MemTotal=15476156kB；CPU/memory/IO PSI可读；zram0约16GiB | 只读路径匹配；不改zRAM、LMKD、MMD |
| 电池 / 屏幕 | battery目录可读；快照67%、Charging、36.2°C；背光与power状态均为息屏/Dozing | 现有解析回放一致；USB充电下不能推断续航收益 |
| 温度 | 84个zone路径；修复后快照保留72个有效温度节点，CPU最高45.9°C | CPU命名可识别；排除已证实的BCL虚拟传感器；HAL缓存值与当前值不同，不能混作同一时刻测温 |
| 前台事件 | `wm_on_*`包含Activity类名；`wm_set_resumed_activity`实际输出包名/Activity | 已改订阅并严格解析显式Component；真实事件主机测试通过 |

### 本次发现与修复

| 等级 / 状态 | 位置 | 证据、影响与处理 |
|---|---|---|
| P1 / 已本地修复 | `daemon/src/collector/appevent.rs:218,357` | `wm_on_resume_called`中的`com.coolapk.market.view.feed.ReplyActivity`被当作包名。设备`/proc/26533/cmdline`实际为`com.coolapk.market`；同应用页面切换会污染前台历史与预测。新增`wm_set_resumed_activity`订阅，只接受明确的`package/activity`，拒绝猜测类名对应包名。两项真实格式回归在修复前失败，修复后通过。 |
| P2 / 已本地修复 | `daemon/src/collector/thermal.rs:24` | `last_ok_ms=0`同时表示无样本和合法首轮时刻。真实快照`--once`在ts_ms=0误报stale_data、CPU温度null，而self-test有有效值。改用Option，零时刻有效且缓存间隔保持正确；回归先失败后通过。 |
| P1 / 已本地修复 | `daemon/src/collector/thermal.rs:129` | HAL中socd/vbat/ibat类型为8/6/7，属于电池比例/电压/电流；原代码把socd按soc关键词归CPU。回归中40°C CPU + socd=98误得98°C；CPU节点缺失时还可能错误信任socd。排除这些节点及BCL级别节点，缺CPU仍拒绝信任；回归先失败后通过。 |
| P2 / 仍待完善 | `daemon/src/collector/cpu.rs:123` | WALT标志只看3个固定路径；本设备3路径均不存在，但scaling_governor实读walt。`walt=false`是探测盲点，不代表设备没有WALT。仅展示项，未修改策略。 |
| P2 / 仍待完善 | `daemon/src/engine.rs:58` | DeviceInfo页大小显示unknown：可读smaps_rollup不含KernelPageSize时未继续查smaps。设备getconf已证实4096；内存采集采用独立sysconf路径，不用该展示字段。 |
| 待实机验收 | SukiSU生命周期、日志子进程、WebUI、厂商共存 | 短时inspection不启动Watcher或常驻Runtime，不能证明持续存活、WebView兼容或回滚有效。维持observe_only和执行硬门关闭。 |

非温度类型依据：[AOSP TemperatureType.aidl](https://android.googlesource.com/platform/hardware/interfaces/+/refs/heads/main/thermal/aidl/android/hardware/thermal/TemperatureType.aidl)，读取blob为`ce2a9fcf7e23dd4a674a34fb4828e4122858849a`；BCL类型6/7/8与设备thermalservice输出对应。本次没有将HAL历史缓存的78.7°C冒充采集时CPU温度。

### 厂商共存与不能下的结论

- 运行进程/服务包含Power HAL、QTI perf2、Xiaomi miperf2/uniperf/mimd2、mi_thermald、poweropt、perfservice、PowerKeeper。**存在这些服务不证明它们正在写同一uclamp节点。** 尚未验证多方所有权与长期冲突情况。
- 模块元数据包含Scene关联组件（Android Auto系统化模块、hideSceneport）。没有据此认定Scene调度正在运行或正在争写；未执行任何第三方模块脚本。
- `/system/bin/mmd`存在；`init.svc.mmd`为空、`init.svc.mmd_setup=stopped`，当次未看到独立mmd进程。Xiaomi `mimd2`在运行，不能等同AOSP MMD。快照未包含system二进制/进程表，回放的`mmd.present=false`不代表设备缺少mmd二进制。
- 文件权限位、Root身份、read-only self-test的`interface_verified`均不证明写入成功或厂商共存安全；`write_probe_performed=false`，`writes_allowed=false`。

### 证据与验证边界

主机证据根目录为：

`C:\Users\Administrator\.codex\visualizations\2026\11\07\01a1b44a-2e50-7f63-ba1f-948615235d5e\xiaomi15-readonly`

其中`capture.py`为只读ADB采集器，`files-raw.json`保留原始字节，`evidence.json`记录238项文件读取成功、7项不可用及探测输出；`identity-and-abi.json`、`events-and-walt.json`、`component-events.json`保留补充命令与原始结果。一个power_supply目录名含冒号，主机映射时编码为`%3A`，原始路径和映射均保留；battery及cpu/thermal路径未改名。采集不是原子快照，不能用不同文件采集时刻作严格同步对比。

`captured-root`仅用于Windows现有程序回放。`self-test-after.json`为ok/0 failures，`status-after.json`首轮温度trusted、CPU45.9°C、72 zones，两次均exit0且未创建unused-state。前台Watcher在inspection模式不启动，前台解析另外用真实日志单测验证；**回放不等于ARM64程序在手机上执行，也不验证连续CPU负载、预测收益或长期稳定。**

随后单独进行了真实ARM64执行：上传后比对设备与主机二进制SHA256，两个只读命令都使用20秒timeout及独立、原本不存在的state路径。`device-self-test.json`记录`root=/`、verdict=ok、failures=0、两簇、72温度节点、CPU41.7°C、battery70%/36.3°C/Charging、mmd.present=true但pid=0；`device-status.json`记录真实设备身份、ksud 4.2.1接口以及各读取项正常。`ACTUATOR_ENABLED=false`、`writes_allowed=false`、`write_probe_performed=false`。没有生成state目录，临时二进制与目录均已删除。全命令、退出码和清理证据保存在`device-run-evidence.json`。此结果证明短时只读程序可执行，不能替代常驻与调度验收。

### 软件交付、回滚与下一步

- 本次修改源码2个文件：appevent.rs修复事件解析，thermal.rs修复首轮时刻及非温度节点；新增4项回归。文档更新本文件、PROGRESS.md、TEST_REPORT.md。未修改默认配置或执行开关。
- 251项Rust测试通过（213库+8CLI+30集成）；fmt、主机/ARM64 all-targets clippy通过；当前源码主机build与ARM64 musl release成功。详细命令见TEST_REPORT.md本次补充。
- 最终本地候选：证据目录下`build-output-verified/neurosched-v0.1.0-dev-arm64.zip`，SHA256=`34701bc1653b4fa8571436af90eea9e1c1b14f9046477a25a12a0d89e0122565`。静态AArch64、PT_LOAD对齐16384，安全默认值验证通过。中间`build-output`不含最后一项传感器修复，不能作为本次最终产物。未安装任何包。
- 本轮修改前5个文件的逐文件原件位于证据目录`before-fix`，恢复对应文件即可回滚本轮本地改动；恢复前需核对后续用户改动。旧repair-manifest只代表上一轮版本，不覆盖本次修改。设备无本轮状态需回滚。
- 下一步：修复WALT/页大小展示盲点；经明确模块安装授权后，在SukiSU下验证模块生命周期与WebUI，再做24–48h只读基线、事件来源及温度质量检查；之后才能讨论受控uclamp和A/B。厂商共存、恢复边界和明确写入批准均为Phase3前置条件。

---

## 2026-11-07 早期非目标设备记录（已被上文补充）

获准执行 `adb devices -l` 和只读身份命令后，连接设备为ZTE MU3356 / UMS9632 / Android15 / SDK35，内核为6.6.98 Android15 4K，ADB为shell UID2000。该设备身份与目标Xiaomi15不同，目标设备的拓扑、cgroup ABI、权限、温度映射和厂商共存均待验证。

当前构建仅完成主机和ELF验证。权限位与挂载匹配可供接口审查，实际可写性须另行批准验证。只读检测工具和目标选择步骤见 [本轮交付](IMPLEMENTATION_2026-11-07.md)。以下保留2026-10-30设备缺失时的设计记录。

---

## 2026-10-30 历史状态

> 以下原始设计记录含已过期的尺度猜测、温度降级等说明，保留用于追溯，不能作为当前实现或设备操作指导。当前结论以上文及CODE_AUDIT/IMPLEMENTATION为准。

**状态：未验证（NO DEVICE）**

| 项 | 结果 |
|---|---|
| 审计时间 | 2026-10-30 12:48 UTC（会话开始） |
| `adb devices` | 空 |
| 实机读取 | **未执行** |
| 结论来源 | 官方文档 + 合成 fixture（`daemon/tests/common/mod.rs`）+ 代码路径分析 |

本文件**不是**对当前这台小米 15 的实测结论。它是"代码在真实设备上会怎么判断"的说明，以及在设备连上后如何一键生成真实报告的方法。

> 任何声称"已在本机验证 CPU 簇/PSI/uclamp 可用"的说法都是假的，本文档刻意不这么做。

---

## 1. 生成真实报告（设备连上后执行）

```bash
# 1) 只读 shell 审计（无需安装模块）
bash scripts/audit.sh                 # 输出 docs/audit-<时间戳>.txt

# 2) 安装模块后，由守护进程自己报告
adb shell su -c "/data/adb/modules/neurosched/bin/neuroschedd --self-test"

# 3) 或从 WebUI 的 Safety 页读取 capability 卡片
```

`--self-test` 是**纯只读**的：它只 `open`/`read`，不写任何节点，返回 JSON，`failures > 0` 时退出码非 0。模块在 `boot-completed.sh` 里会自动把结果写到 `/data/adb/neurosched/logs/capability.json`。

---

## 2. 代码如何判定能力（与设备无关的逻辑）

| 能力 | 判定方式 | 缺失时的行为 |
|---|---|---|
| CPU 簇 | 枚举 `/sys/devices/system/cpu/cpufreq/policy*`，读 `related_cpus`；无则回退 `cpu*/cpufreq` | 无簇 → `cpu` 采集器标记 `ok=false`，策略退化为 BALANCED，不写入 |
| 簇命名 | 按 `cpuinfo_max_freq`（再按 `cpu_capacity`）降序排名 → `prime/gold/silver/bronze` | **绝不按 CPU 编号猜测**；单簇设备就叫 `prime` |
| 频率 | `scaling_cur_freq`；为 0 时回退到所属簇的值 | 显示 `0 MHz`，不参与决策 |
| WALT | 存在 `/proc/sys/kernel/sched_walt_rotate_window` 或 `/sys/module/walt` | 仅作展示，不参与决策 |
| uclamp | 依次探测 `/sys/fs/cgroup/top-app`、`/dev/cpuctl/top-app`、`/sys/fs/cgroup/cpu/top-app` 下的 `cpu.uclamp.min/max` | 全部缺失 → `uclamp: null`，执行器空转，模式仍可切换但不产生写入 |
| uclamp 尺度 | 读当前值：任一 >100 → `perf_0_1024`，否则 `percent_0_100` | 无法解析 → 视为不可用（不猜） |
| cgroup 版本 | `/sys/fs/cgroup/cgroup.controllers` 是否存在 | 仅影响节点路径选择 |
| PSI | `/proc/pressure/memory` 可读且含 `some`/`full` | 不可用 → 内存压力只靠 `MemAvailable` + `kswapd0`，并在 health 里标记 `psi: fail` |
| zRAM | `/sys/block/zram*/mm_stat`，字段 ≥3 个才解析 | 缺失 → `zram: []`，压缩比显示 `—` |
| MMD | `init.svc.mmd`（getprop，仅启动时一次）+ `/proc/*/comm == mmd` + `/system/bin/mmd` 存在 | 任一命中即 `present`；**不重建、不接管** |
| 温度 | `/sys/class/thermal/thermal_zone*/{type,temp}`，按 `type` 分类 | 无 CPU 类 zone → `cpu_c: null`，热策略视为 NONE（不封顶），但 UI 明确显示"无传感器" |
| 电池温度 | `/sys/class/power_supply/battery/temp`（十分之一度） | 缺失 → 回退到 battery 类 thermal zone |
| 屏幕状态 | `/sys/class/backlight/*/brightness`，回退 `/sys/class/leds/*backlight*` | 都缺失 → `unknown`，IDLE 不触发（保守） |
| 前台应用 | `logcat -b events` → companion 日志 → `/proc/*/cgroup` 含 `top-app` | 全失败 → `source=none`，预测器拒绝预测，策略退化为 BALANCED |

---

## 3. 小米 15 / 8 Elite 的预期形态（**假设，待实测**）

以下是基于公开规格与 Android 通用布局的**预期**，不是实测值：

| 项 | 预期 | 验证方式 |
|---|---|---|
| 簇数 | 2（`policy0` 6 核 + `policy6` 2 核） | `audit.sh` 第 4 节 |
| prime 簇 | `cpu6-7`，`cpuinfo_max_freq ≈ 4.32 GHz` | 同上 |
| performance 簇 | `cpu0-5`，`cpuinfo_max_freq ≈ 3.53 GHz` | 同上 |
| governor | `schedutil`（HyperOS 可能替换为厂商 governor） | 同上 |
| uclamp | cgroup v2 `/sys/fs/cgroup/top-app/cpu.uclamp.{min,max}`，**尺度待定** | `--self-test` 的 `actuator.uclamp.scale` |
| PSI | 开启（GKI 默认 `CONFIG_PSI=y`） | `audit.sh` 第 7 节 |
| MMD | Android 17 上应存在，`init.svc.mmd=running` | `audit.sh` 第 8 节 |
| zRAM | `/dev/block/zram0`，`mm_stat` 可读 | `audit.sh` 第 6 节 |
| 温度 zone | 至少含 `cpu-*`、`gpuss-*`、`battery`、`charger`、`skin-therm` | `audit.sh` 第 9 节 |
| 屏幕 | `panel0-backlight` | `audit.sh` 第 3 节间接 |

**特别提醒**：`cpuinfo_max_freq` 在部分 HyperOS 版本上会被厂商限制为"可用上限"而非硬件上限，届时 `prime` 的判定仍成立（相对排序不变），但显示的 MHz 会低于规格值。这是正常现象，代码不做修正。

---

## 4. 已知的未知（必须实机确认）

1. **uclamp 尺度**：0..100 还是 0..1024。两者代码都支持，但只有实机能确定走哪条分支。
2. **uclamp 是否可写**：KernelSU root 能否写 cgroup 节点，取决于 SELinux 域与 cgroup 挂载权限。`--self-test` 只判断"存在且可读"，**可写性只能在启用模式后由第一次写入 + 回读验证**。
3. **HyperOS 是否有自己的调度服务**（如 `miui boost`、`perfd`）在同一节点上周期性写入。若有，本项目会在第一次检测到外部写入时**主动释放该节点**，功能自动降级为只监控 —— 这是设计行为，不是故障。
4. **MMD 的实际开关状态**：`mmd.zram.enabled` 是否为 true，决定 zRAM 是否由 MMD 全权管理。
5. **logcat events 的 tag 名**：`wm_on_resume_called` / `wm_on_top_resumed_gained_called` 在不同 Android 版本有差异，代码同时匹配两者，但 HyperOS 可能裁剪。若都不命中，会回退到 `/proc` 扫描（置信度上限 50%）。
6. **温度传感器的 `type` 命名**：分类器基于关键字（`cpu`/`soc`/`gpu`/`skin`/`batt`/`charger`/`disp`）。若 HyperOS 用了完全不同的命名，会落到 `other`，此时 `cpu_c` 为 null，热保护不生效 —— **这是安全侧失败**，会明确显示在 UI 上。

---

## 5. 与 Scene 的共存

| 场景 | 行为 |
|---|---|
| Scene 只读监控 | 无冲突 |
| Scene 也写 `top-app/cpu.uclamp.*` | NeuroSched 检测到"当前值 ≠ 自己最后写入值" → 记 conflict → **释放该节点**，本次会话不再写它 |
| Scene 写 CPU 频率 / 温控 / zram | 本项目不碰这些节点，无冲突 |
| 卸载 NeuroSched | 只回滚自己写过的节点；已被外部改写的节点不动 |

安装与卸载**都不会**关闭、卸载或修改 Scene。

---

## 6. 结论

- 代码对"接口不存在"的处理是**降级而非崩溃**，每一条降级路径都有单测覆盖（见 `docs/TEST_REPORT.md`）。
- 但"接口存在且语义如预期"这件事，**只有实机能证明**。
- 因此默认安装为 `observe_only`：先跑 24–48 小时，看 `capability.json` 与 Dashboard，再决定是否进入 `dry_run`/`enabled`。

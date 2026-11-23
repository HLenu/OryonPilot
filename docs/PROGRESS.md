# 进度记录

## 2026-11-23 v0.1.2-observe.1 实机巡检与完善交付

代码基线已更新至 **OryonPilot `v0.1.2-observe.1`（`versionCode=103`）**。本轮先连接目标小米 15（`dada` / `8d053a1f`）完成 17 天真实观测数据只读巡检，随后完成 4 项核心修复与预测评估完善，通过全套主机/交叉构建测试并在目标设备上完成隔离只读 smoke 验证。当前状态仍保持安全默认值 `mode="observe_only"`、`actuator.enabled=false`、`ACTUATOR_ENABLED=false`；设备侧尚未覆盖安装新版（仍在运行 `v0.1.1-observe.1`），等待用户自行安装升级并观察。

### 实机巡检实证（v0.1.1-observe.1，2026-11-07 至 2026-11-23）

- 设备已安装 `v0.1.1-observe.1 (102)`（SHA256 一致），无 `disable`/`remove`/`stop` 标记，`actuator.enabled=false`、`journal/` 全部 `.jsonl` 为 0 字节，`ro.boottime.init.fsck.data=119`（ms）。
- `neuroschedd` 连续运行约 19.6 h 稳定无崩溃（`VmRSS=1,348 kB`、`Threads=16`、累计 CPU `05:19`，`127.0.0.1:8787` 监听正常）。
- 累计 `history.csv` 14,790 行（17 天，CPU die 温度 P50=43.6°C / P75=49.0°C / P90=58.3°C / P95=66.8°C / P99=93.0°C），`model.json` 约 330 KB（20,261 次前景事件，147 个包，v2 统计在含桌面干扰下为 6,769/13,159）。
- 定位 4 类实测缺陷：`PendingRecovery::PreviousBoot` 未区分空 journal 导致自 11-11 重启起 11,472 条记录（77.6%）策略状态卡死在 `SAFE_MODE`；骁龙 8 Elite 45/50/56/62°C 温控阈值过低且 `ThermalGuard` 滞回退出锚定 `enter_light_c` 与忽略 `skin_c`；`Predictor` 将桌面（如 `com.miui.home` 8,178 次）混入转移链并缺少同机会基线/覆盖率/拒绝原因对照；模型每 5 分钟无脏检查全量 `to_string_pretty` 重写且无包数上限。

### 本轮改进与验证证据

1. **修复跨重启空 journal 误锁 `SAFE_MODE`**：`safety/transaction.rs` 在 `PendingRecovery::PreviousBoot` 中精确标注 `empty: bool`（仅严格 0 字节为 true，残缺记录仍为 false）；`engine.rs::Runtime::new` 与 `main.rs::RestoreAll` 对空日志直接安全关闭（`journal.end()`），仅非空/残缺跨 boot 日志归档并触发 `SAFE_MODE`。
2. **校准 8 Elite 温控阈值与滞回一致化**：默认 `policy.thermal` 校准为 `65 / 75 / 83 / 90°C`（`exit_margin_c=5.0`）；`ThermalGuard` 统一 `effective_temp`（同步支持 `include_skin`），滞回退出门限修正为 `enter_moderate_c - exit_margin_c`；`api/mod.rs` 与 `webui/app.js` 补全 `enter_critical_c` 配置项。
3. **预测器去桌面化、同机会基线对照与存储收敛**：`Predictor` 注入 `apps.launchers` 忽略列表（跳过去重、回退真实上一应用作桌面期间上下文、`usage_ranking` 与 `PredictionPreview` 同步过滤），新增同机会 `opportunities` / `refused` / `refused_reasons` / `top3_hits` / `effective_accuracy` 及高频（`baseline_most_frequent_hits`）与回切（`baseline_previous_hits`）基线对照，`stats_version` 升级至 `3`（保留模型结构并将 v2 统计归档至 `legacy_stats`）。模型持久化增加 `dirty` 检查、256 包有界裁剪、`to_string()` 紧凑写盘与 `0600` 权限加固（同步加固 `daemon.pid` 与 `boot.json`）；WebUI 预测页补充覆盖率、Top-3 与基线对比展示。
4. **验证与构建产出**：最后一次代码编辑后执行，**296 项 Rust 测试**（253 库单测 + 8 CLI + 35 集成）全数通过，**24 项 Python 打包测试**通过，主机及 `aarch64-unknown-linux-musl` `clippy -- -D warnings`、`cargo fmt --check`、`node --check webui/app.js` 与 `python -X utf8 -B scripts/build.py --check` 全部通过。
   - 交付包：`dist/neurosched-v0.1.2-observe.1-arm64.zip`（565 KiB，SHA256 `4e7b99aca58c75f400647a9ba53ff28927eeee9b8f28cf5d07dfda761a038dfa`）
   - 校验文件：`dist/SHA256SUMS`；包内 `bin/neuroschedd` 静态 AArch64 ELF（997 KiB，16 KiB PT_LOAD 对齐，SHA256 `62d2dcb80431f779b1f214d2490d0e2af9f7536d5acd6100a56dda76ffadb57c`）
5. **小米 15 隔离只读 smoke 验证**：推送新版二进制至 `/data/local/tmp/oryonpilot-012-check`（SHA256 校验一致），使用不存在的 `--data-dir /data/local/tmp/oryonpilot-012-unused` 执行 `--version`（`0.1.2-observe.1 (103)`）、`--self-test`（`verdict="ok"`, `failures=0`, `ACTUATOR_ENABLED=false`, `walt=true`, `uclamp=true`）与 `--once`（`safe_mode=false`, `state="BOOT"`, `thermal.level="NONE"`）；确认未创建临时目录并已彻底删除临时二进制，未触碰 `/data/adb/neurosched` 与常驻进程。

### 下一步（用户操作）

- 在 SukiSU Ultra 从本地选择 `dist/neurosched-v0.1.2-observe.1-arm64.zip` 覆盖升级后，在方便时用系统电源菜单正常重启使 `v0.1.2-observe.1` 生效。
- 升级保留既有 `/data/adb/neurosched/config.json` 与 `model.json`（模型自动迁入 v3 评估）；若希望已有的旧配置立即采用新默认温控阈值（`65 / 75 / 83 / 90 °C`、回差 `5 °C`），可在 WebUI “策略”页修改保存，或手动备份后删除旧 `config.json` 再重启生成默认配置。
- 升级生效并经历一次正常重启后，验证 `SAFE_MODE` 不再被空 journal 误锁、`stats_version=3` 真实评估及同机会基线对比数据，再评估 Phase 3 执行资格。

---

## 2026-11-23 交接入口

交接已整理。代码基线仍为 **OryonPilot v0.1.1-observe.1（102）**，项目日用验收为 **PARTIAL**。接手时从本节开始，下面各节保留当时的证据与时间边界。

### 当前状态

- 本轮按发布清单重算 98 个文件的 SHA256，全数匹配。在 Rust 源码与测试、vendor、模块、WebUI、脚本、文档及 Companion 目录中，写入本节前也没有发现清单外文件。工作区无 Git 元数据，使用版本包和哈希追踪基线。
- 现有安装包、源码归档和优化前快照均通过 SHA256 核对。当前安装包位于下述交付目录的 `build-output`。工作区 `dist/` 只存有旧的 `neurosched-v0.1.0-dev-arm64.zip`。源码归档是 11 月 7 日发布快照，最新交接内容以工作区本文件为准。
- 默认模式 `observe_only`，`actuator.enabled=false`，生产硬门 `ACTUATOR_ENABLED=false`。保留模块 ID `neurosched`、可执行文件 `neuroschedd` 和数据目录 `/data/adb/neurosched`。本轮仅更新本交接文档，未修改源码、打包或运行设备上的程序。
- 11 月 7 日的 289 项 Rust、24 项打包测试、ARM64 构建及小米 15 隔离只读运行证据仍对应同一份源码。此次没有重跑测试。11 月 23 日主机侧 `adb devices -l` 成功返回空列表。当前安装版本、是否正在运行、24–48 小时稳定性均待核实。

### 文件、命令与修改位置

交付目录为 `C:\Users\Administrator\.codex\visualizations\2026\11\07\01a1b64d-049a-74a1-a677-65cdb87f9bf8`。

| 用途 | 位置 |
|---|---|
| 当前 SukiSU Ultra 安装包 | `build-output/neurosched-v0.1.1-observe.1-arm64.zip` |
| 包的 SHA256 | `07746e2b10c64499b5294de622865eaec3d35541f7b7c258d5daece4c69d336d` |
| 发布源码与逐文件校验 | `OryonPilot-v0.1.1-observe.1-source.zip`、`optimization-manifest.json` |
| 源码回滚原件 | `OryonPilot-before-optimization.zip`、`before-manifest.json` |
| 实机与构建证据 | `build.log`、`device-self-test.json`、`device-diagnostics.json`、`device-smoke-evidence.json` |
| 构建环境、命令、版本验证、停用与回滚 | [发布交付记录](OPTIMIZATION_2026-11-07.md) |

常用入口为 `daemon/src/engine.rs`（主循环、状态发布）、`collector/`（采集）、`predictor/` 与 `policy/`（预测和策略模拟）、`safety/`（事务与监控）、`api/` 与 `webui/`（本地控制台）、`module/service.sh`（模块启动与守护）。HTTP 的 vendored 补丁来源和限制记录在 [PATCHES.md](../daemon/vendor/tiny_http/PATCHES.md)。目标设备和接口证据见 [DEVICE_CAPABILITY.md](DEVICE_CAPABILITY.md)。

改动源码后优先按影响范围运行 `cargo test --manifest-path daemon/Cargo.toml --locked --offline`，出包用 `python scripts/build.py --check`。Windows 上先按发布记录配置工具路径和独立构建目录。构建脚本默认重建并更新 `dist/`，需要留档时使用独立输出目录。内核写入、刷模块、重启、系统分区和厂商服务操作需沿用用户明确授权边界。

### 按顺序接手

1. **确认设备现况。** 小米 15 再次连接后，先通过 `adb devices -l` 核对目标，再读模块版本、进程状态和生效模式。旧记录的目标序列号为 `8d053a1f`，设备代号为 `dada`。核实升级状态前，不把 11 月 7 日临时运行新版当作已安装新版的证据。用户自行安装升级和重启，升级保留已有配置及停止标记。
2. **验收观察版。** 验证 SukiSU WebUI 登录和脱敏导出、连续 24–48 小时采样、亮屏/息屏资源开销、异常重启和正常停止。保留版本号、采集时间、轮数、错误计数及脱敏摘要。诊断快照只代表采集时刻。旧版首次重启时的约 73.88 秒延迟发生在 Daemon 启动前的 /data F2FS 检查，证据见下文。若再次发生，比较开机记录再判断。
3. **评估预测。** 用只读数据比较模型与简单基线，记录覆盖率、命中率、拒绝原因及样本量，同时标明多用户、分屏和事件来源缺失等未验证边界。
4. **评估实际执行资格。** uclamp 可读已验证，可写性、Power HAL/厂商服务共存、实机恢复和性能/温度/功耗 A/B 仍待验证。当前以只读观察为验收目标。若模块异常，优先在 SukiSU 停用并保留数据；需要即时停止时，使用交付记录中的协作停止命令。新版事务格式应与旧版分开核查，保留恢复日志。

## 2026-11-07 v0.1.1-observe.1 优化交付记录

本轮观察测试包已交付准备完成，日用目标仍为PARTIAL。实现boot_id恢复边界、独立主循环活性监控、健康样本判断、长间隔stop响应、HTTP连接/时间/大小/线程边界、WALT/页大小/MMD探测修正、Daemon资源指标与鉴权脱敏导出。保留observe_only及关闭生产写入硬门。详情、文件清单、测试/风险/安装/回滚见 [本轮优化交付](OPTIMIZATION_2026-11-07.md)。

最终289项Rust（247库+8CLI+34集成）、24项构建测试通过，fmt与主机/ARM64 clippy通过，JS语法通过。ARM64静态release重建，17条目ZIP校验通过；当前包为`neurosched-v0.1.1-observe.1-arm64.zip`，SHA256 `07746e2b10c64499b5294de622865eaec3d35541f7b7c258d5daece4c69d336d`，位于本轮artifact根`01a1b64d-049a-74a1-a677-65cdb87f9bf8/build-output`。

同一目标小米15上新版self-test/once exit0；临时隔离Runtime连续3帧诊断正确、鉴权401/200正确、资源有效、WALT=true/4kB页大小、自己的写journal为0字节，正常停止约0.589秒并确认logcat子进程退出。临时程序、数据与转发清理成功。首次检测脚本因内核缺children文件中断且清理成功，改为PID/PPID后通过。没有安装本次新包、重启手机、操作旧模块或写调度参数。

下一验收：用户自行升级，24–48小时只读采集与WebView/生命周期；再评估预测质量和简单基线。累计sample_errors=1而3帧当次健康均ok，具体启动期来源未保留，后续持续观察。新包长期功耗、预测收益、A/B与真实恢复均未验证。先前用户已安装旧包及fsck诊断事实保留如下，不能把下方“未安装”的历史段落当作当前设备状态。

## 2026-11-07 22:01 首次安装后的第一屏延迟调查

用户已手动安装交付包，并使用SukiSU安装完成后的重启按钮。本轮只读诊断，未停止模块、修改设备文件/参数、执行fsck或重启。手机已正常进入系统。

**直接延迟已定位到/data的F2FS开机检查：73.880506秒。** `ro.boottime.init.fsck.data=73891ms`，内核记录late mount在启动后2.744秒至77.619秒执行，耗时74874ms并成功；保留的上一轮开机日志同项为892ms。这发生在模块启动前，OryonPilot Daemon从`/proc/12578/stat`和CLK_TCK=100计算于启动后118.28秒开始运行，日志对应北京时间21:54:24。

`/dev/fscklogs/fsck`记录检查前checkpoint含`orphan_inodes sudden-power-off`，检查了约220万个节点并修复3个quota记录；第二次检查记录`unmount`及`No error was reported`。可确认上次未形成干净卸载状态和本次恢复检查，不把该标志等同于用户物理断电，也不凭用户使用SukiSU重启就断言管理器实现有故障。缺少上次完整关机轨迹，重复性未测试。

安装的二进制及service/boot-completed脚本哈希与交付包一致；模块无post-fs-data脚本。运行时日志未见panic/反复重启，PID12578保持一致，history.csv持续更新至22:00:37，前台记录随应用切换；配置为observe_only、actuator.enabled=false，事务journal为0字节。由此确认首次开机的supervisor与短时持续采集已运行；完整生命周期、WebUI、24–48h稳定性仍未验收。启动后曾记录CPU高温，不能把后续温度记录当作第一屏延迟原因。

证据目录：`C:\Users\Administrator\.codex\visualizations\2026\11\07\01a1b44a-2e50-7f63-ba1f-948615235d5e\xiaomi15-freeze-20261107-215609`。`key-evidence.txt`是精简原文，`fsck_detail.json`、`previous_fsck.json`、`boot_properties.json`、`daemon_state.json`保存命令与输出，`diagnosis.json`保存结论及哈希。本轮没有源码修改，无需重建或重跑代码测试。

后续在用户自行选择重启时，用系统电源菜单正常重启作对照，比较同样的fsck/late mount指标；未经批准不代为重启，不通过关闭fsck或修改fstab绕过检查。若仍重复长检查，继续追查关机/文件系统记录。以下接管状态中的“未安装模块”描述保留为首次安装前历史。

## 2026-11-07 接管修复状态

当前结论为 PARTIAL。目标Xiaomi15已只读连接核实：dada/SM8750、Android17/HyperOS4、6.6.118内核；SukiSU Ultra授权Shell后Root读取成功。基于真实快照和日志新增4项回归，修复前台类名误当包名、首轮温度时间戳、BCL虚拟传感器误当温度，共251项Rust测试通过，ARM64 musl重建通过。临时上传程序后只运行self-test/once，两者在目标设备exit0、自检0 failures，未创建state目录，临时文件已清理。未安装模块、启动常驻服务、写内核节点或重启；日用、长期稳定和收益验收待完成。详情见 [设备能力当前记录](DEVICE_CAPABILITY.md)。

| 阶段 | 当前状态 | 下一验收 |
|---|---|---|
| Phase0 | 已修复已复现的鉴权、事务、进程锁和恢复问题，主机验证 | boot_id恢复边界、独立活性检测、KernelSU生命周期 |
| Phase1 | 已验证真实拓扑、cgroup ABI、Root读取；真实快照回放与ARM64短时只读执行通过 | 常驻与WebUI、WALT/页大小展示盲点、24–48h只读基线 |
| Phase2 | 规则/预测修复与未来目标预览通过主机测试；真实前台事件格式已适配 | 实机事件连续性、预测结算分布与收益指标 |
| Phase3 | 生产硬门关闭 | 接口、厂商共存、恢复协议与明确批准 |
| Phase4–5 | 待实施 | 收益反馈、A/B、长稳与发布 |

安全默认值为 `mode="observe_only"`、`actuator.enabled=false`、`ACTUATOR_ENABLED=false`。人工输入WebUI凭证、卸载保留数据、stop跨重启保留、强制当前源码构建、旧预测统计隔离保留，均已记录兼容性决定。

前一轮接管文件、命令、故障证据、初期快照局限及恢复映射见 [接管交付](IMPLEMENTATION_2026-11-07.md)；其中目标设备缺失、247项测试及包哈希是设备连接前状态。本次源码、文档5文件原件保存在xiaomi15-readonly/before-fix，最终包位于该证据目录build-output-verified。下文保留2026-10-30历史记录，当时的完成声明、测试数和设备状态仅属于该日期。

---

## 2026-10-30 历史记录

版本：v0.1.0
最后更新：2026-10-30
总体状态：**源码、测试、ARM64 模块包、文档全部完成；实机验证未进行（设备未连接）**

---

## 阶段表

| # | 阶段 | 状态 | 产出 / 证据 |
|---|---|---|---|
| 1 | 检查工作区 | ✅ | 工作区为空，无旧工程 |
| 2 | 检查 ADB 设备 | ⚠️ 无设备 | `adb devices` 返回空；全程未出现设备 |
| 3 | 检查 Root 与系统信息 | ⛔ 未执行 | 依赖设备；改为提供 `scripts/audit.sh` + `--self-test` |
| 4 | 检索开源项目与官方资料 | ✅ | `docs/OPEN_SOURCE_RESEARCH.md`（含许可结论、版本、最后提交） |
| 5 | 事实清单与接口能力矩阵 | ✅（合成） | `docs/DEVICE_CAPABILITY.md`，明确标注"未验证" |
| 6 | 架构设计 | ✅ | `docs/ARCHITECTURE.md` |
| 7 | 建立测试环境 | ✅ | `daemon/tests/common/mod.rs` 合成设备树 + `SysRoot` 路径注入 + 故障注入 |
| 8 | 只读监控核心 | ✅ | `collector/*`（cpu/memory/pressure/thermal/battery/process/appevent） |
| 9 | 策略模拟 | ✅ | `policy/*`（状态机 + 热/内存守卫 + 优先级裁决 + 四重防抖） |
| 10 | 安全执行器 | ✅ | `actuator/*`（八步事务 + 所有权 + 回读校验 + 回滚 + 尺度自适应） |
| 11 | UI | ✅（未在真机 WebView 验证） | `webui/{index.html,app.js,style.css}` + `module/webroot/index.html` |
| 12 | 测试 | ✅ 主机侧 / ⛔ 实机 | 135 passed / 0 failed；`docs/TEST_REPORT.md` |
| 13 | 构建安装包 | ✅ | `dist/neurosched-v0.1.0-arm64.zip`（434 KiB，SHA256 见 `dist/SHA256SUMS`） |
| 14 | 最终报告 | ✅ | 本文件 + 各 docs + README |

图例：✅ 完成 / ⚠️ 受限完成 / ⛔ 无法执行

---

## 关键决策记录

| 时间 | 决策 | 理由 |
|---|---|---|
| 启动 | 零外部 crate（纯 std） | 单文件 783 KiB 静态二进制；无供应链风险；无需 NDK |
| 启动 | 交叉编译用 `aarch64-unknown-linux-musl` + `rust-lld` | 本机无 Android NDK；musl 静态 ELF 可直接在 Android 运行 |
| 启动 | 许可选 GPL-3.0-or-later | 与生态内 GPL 项目兼容；未复制任何代码 |
| 设计 | `BALANCED` 作为兜底而非竞争者 | 否则其 30 分会压过 `IDLE`(20)，息屏永不进入省电态 |
| 设计 | uclamp 尺度运行时探测 | 内核文档明确 cgroup=0..100、sysctl=0..1024，写错会被 EINVAL |
| 设计 | 应用事件三级降级 + `source_trust` | logcat 可能被裁剪；`/proc` 兜底不可靠，用置信度表达而非假装准确 |
| 设计 | 温度分级自算，不读 `dumpsys thermalservice` | 1 Hz 读 dumpsys 需 binder+shell，代价高于收益；UI 标注 `derived` |
| 设计 | 不实现 zRAM/LMKD/swappiness 调优 | MMD 已接管；与系统对赌收益不可证 |
| 测试 | 用 fixture 而非 mock 框架 | 路径注入更接近真实行为，能覆盖"文件存在但内容异常" |

---

## 与需求的偏差（全部有意，已记录）

1. **`BALANCED` 优先级**：需求表把 `BALANCED` 列在 `IDLE` 之前；实现中 `BALANCED` 只在无其它信号时使用。
2. **`sched_util_clamp_*` 默认关闭**：需求允许"已确认不会与系统控制冲突的接口"，但全局 clamp 影响所有任务，故列为 opt-in。
3. **温度分级来源**：需求要求检查 `dumpsys thermalservice`；守护进程内改为自算（`audit.sh` 仍会 dump），避免 1 Hz binder 开销。
4. **Kotlin Companion App**：未提供可编译 APK（本机无 Android SDK）。已实现其数据通道（`fg_events.log` 增量读取）与文档，应用侧作为可选件留待用户构建。
5. **WebUI 的 KernelSU WebView 兼容性**：未在真机验证；`webroot/index.html` 采用 iframe 嵌入 `127.0.0.1:8787` 的方式，若 WebView 限制混用源，可直接用浏览器打开。

---

## 下一步（唯一需要用户参与的事项）

**把手机连上电脑并授权 ADB**，然后执行：

```bash
bash scripts/audit.sh
```

产出 `docs/audit-<时间戳>.txt`。之后我才能：
1. 用真实值替换 `docs/DEVICE_CAPABILITY.md` 中的"预期"；
2. 确认 uclamp 尺度、可写性、MMD 状态、温度 zone 映射；
3. 启动 24–48 h 基线采集（`docs/BENCHMARK.md` 阶段 A）。

在此之前，任何"实机已验证"的说法都不成立。

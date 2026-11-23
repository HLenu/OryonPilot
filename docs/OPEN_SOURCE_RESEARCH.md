# 开源项目与官方资料调查

调查时间：2026-10-30（UTC）
调查方式：GitHub REST API（仓库元数据、tags、releases、文件树）+ raw 源码阅读 + Android/Linux/KernelSU 官方文档。

> **结论先行**
> 1. 四个参考项目中，`Steambot12/Adaptive-Performance` **没有任何 License 文件**，按默认版权保留处理，**一行代码都没有参考**，只记录了它的功能边界。
> 2. 其余三个项目的许可与本项目（GPL-3.0-or-later）兼容，但本项目**没有复制任何源码**：只借鉴了工程结构、生命周期与失败处理思路，实现全部重写（Rust 零依赖、单文件静态 ELF）。
> 3. 真正需要"不要重复造"的部分是 **MMD**（Android 17 的 zRAM 管理守护进程）。本项目只监控它，不接管 zRAM/swap/swappiness/LMKD。

---

## 1. ylh440104/KernelSU-Tuner

| 项 | 值 |
|---|---|
| 仓库 | https://github.com/ylh440104/KernelSU-Tuner |
| 最后提交 | 2026-10-26 |
| 最新版本 | v3.4.2（2026-10-26 发布） |
| License | **GPL-3.0** |
| 语言 / 体积 | Shell，约 287 KB |
| Android 兼容 | 通用（Magisk/KernelSU 模块，无 ABI 二进制） |
| KernelSU 兼容 | 是，含 `module.prop` / `customize.sh` / `service.sh` / `uninstall.sh` / `webroot/` / `action.sh` / `sepolicy.rule` |
| 依赖 | 纯 shell（`sed`/`awk`/`getprop`/`dumpsys`），无编译产物 |

**目录结构（31 个文件）**：`bin/{tuner,autopilot,guard,appctl,appwatch,custom,display,wipeshield,backup,presets,kernel,watchdog,cfg,init-apply}.sh`、`initrc/KernelSU-Tuner.rc`、`webroot/index.html`。

**值得借鉴的机制**

| 机制 | 它的做法 | 本项目的取舍 |
|---|---|---|
| 模块生命周期 | `customize.sh` 建目录+写默认配置（仅当不存在）+`set_perm`；`service.sh` 读配置后按开关拉起多个子脚本 | 采纳"仅当配置缺失才写默认值"和"安装不改变任何内核参数"；**不采纳**多脚本并行，改为单守护进程 + 单一状态机 |
| 参数备份/恢复 | `backup.conf` 以 `path\|value` 行存储，`uninstall.sh` 逐行回写 | 采纳"记录原始值"的思想；改为 **JSONL 事务日志 + 回写校验 + 外部修改检测**（见 `docs/SAFETY.md`） |
| 看门狗 | `watchdog.sh` 每 30 s 检查各子脚本 pid，缺失则重启；pid 文件存于 `/data/adb/ksu_tuner` | 采纳"pid + 存活检查 + 定期重启"；额外加入**连续启动失败计数 → SAFE_MODE**（它没有） |
| WebUI | 单个 `webroot/index.html`，通过 `ksu.exec` 调 shell | 采纳"单文件、无框架、无 CDN"；**不采纳**从 WebUI 执行任意 shell（本项目 WebUI 只能调用受限 HTTP API） |
| Dry Run | 有"变更预览" | 采纳并强化为独立 `dry_run` 运行模式，报告逐节点 `old → new` |
| 冲突处理 | 无显式所有权概念 | 本项目新增节点所有权 + 外部写入检测 + 主动释放 |

**不适合本设备 / 不采纳的部分**

- 温控分区"解锁"、`THERMAL_OPEN`：直接违反本项目"禁止修改温控阈值"的红线。
- 修改 `min_free_kbytes`、`swappiness`、LMKD、ZRAM 大小：与 MMD 职责重叠，且 Android 17 上 `mmd.zram.enabled=true` 后 `swapon_all` 的 zram 配置会变成 no-op。
- CPU 频率/核心 online 直接写入：v1 明确禁止。
- `chmod 777`、`SELINUX_PERM` 类开关：本项目不允许。

**已知问题（阅读源码可见）**：多个后台 shell 循环（autopilot/guard/appwatch/watchdog）互相独立，无统一状态，容易出现"两个脚本写同一节点"；`watchdog.sh` 用 10 s×30 的 `sleep` 轮询，无法感知自身被 kill 后的重启；配置为 `key=value` 平铺，无版本迁移机制。

---

## 2. Fawrz/Thrawl

| 项 | 值 |
|---|---|
| 仓库 | https://github.com/Fawrz/Thrawl |
| 最后提交 | 2026-08-14 |
| 最新版本 | v1.1.0（2026-06-21 发布） |
| License | **MIT** |
| 语言 | Rust（`libc` 唯一依赖）+ Shell |
| 构建 | `cargo-ndk` + Android NDK 28，目标 `aarch64-linux-android` / `armv7-linux-androideabi`，`opt-level="z"`, `lto=true`, `panic="abort"`, `strip=true` |
| Android 兼容 | Magisk 模块（非 KernelSU 专用，但结构兼容） |
| 依赖 | NDK 交叉编译链；运行时无依赖 |

**源码结构**：`src/{main,main_loop,config,psi,swap,swappiness,zram,vm_controller,lmkd,uffd,logging,legacy,flags,command}.rs`，约 60 KB Rust。

**值得借鉴的机制**

| 机制 | 它的做法 | 本项目的取舍 |
|---|---|---|
| PSI 读取 | `read_avg60()` 逐行 `strip_prefix("some ")` 取 `avg60`；另有 `open_psi()` + `poll(POLLPRI)` 做事件等待 | 采纳"直接读 procfs、不 fork"；本项目解析 `some`/`full` 全字段（avg10/60/300/total），并把 `full` 作为 thrashing 判据；**未采纳** `poll()` 事件等待——1 Hz 轮询 + 缓存已足够，事件等待在低端内核上易误触发 |
| 滞回控制 | 高/低阈值成对出现，避免抖动 | 采纳并扩展为"进入阈值 / 退出阈值 / 最短驻留 / 冷却时间"四件套 |
| 日志 | 精简分级日志 + 轮转 | 采纳"内存环形 + 文件轮转"；增加 WebUI 实时读取与 CSV 导出 |
| 单 daemon 结构 | 一个 Rust 进程替代多个 shell 循环 | 完全采纳（这是本项目架构的直接依据） |

**明确不采纳**：Thrawl 的核心目标是**主动调内存参数**（swappiness、zram 大小、LMKD、uffd）。本项目 v1 一行都不写这些节点，因为：
- Android 17 的 MMD 已经负责 zRAM 重压缩/写回/预取；
- LMKD 参数由厂商 `lmkd` 与 `ro.config.*` 决定，改它等于与系统对赌；
- 这些参数的收益无法在无实机 A/B 的情况下证明。

**已知问题**：`psi.rs` 只取 `avg60`，对 1 s 级突发不敏感；`vm_controller` 直接写 `/proc/sys/vm/*`，卸载时依赖备份回写，若中途被 kill 可能留下半改状态（无事务日志）。

---

## 3. Steambot12/Adaptive-Performance

| 项 | 值 |
|---|---|
| 仓库 | https://github.com/Steambot12/Adaptive-Performance |
| 最后提交 | 2026-08-16 |
| 最新版本 | v1.3（2026-08-16 发布） |
| License | **无**（GitHub API 返回 `license: null`，仓库内无 LICENSE 文件） |
| 语言 | Shell，约 240 KB |
| 功能 | 检测到游戏时切换 CPU governor |

**许可结论（重要）**：没有 License 意味着**保留所有权利**。默认版权法下，复制、修改、再分发都需要作者明示授权。因此：

- **没有阅读其实现细节用于移植**，只依据仓库描述与目录名了解功能边界；
- 本项目的游戏识别、前台应用识别**全部独立实现**（`logcat -b events` 事件解析 + `/proc` top-app 兜底 + 用户可配置游戏包名）；
- 如果未来要参考其代码，必须先取得作者书面授权，或等待其添加明确许可。

**从公开描述可推断的设计局限**（作为反面参考）：单一"检测到游戏 → 改 governor"的二元逻辑，没有滞回、没有温控优先级、没有回滚，遇到厂商 governor 不允许写入时会静默失败。本项目因此把"状态机 + 优先级 + 回滚"作为硬性设计。

---

## 4. VR-25/zram-swap-manager

| 项 | 值 |
|---|---|
| 仓库 | https://github.com/VR-25/zram-swap-manager |
| 最后提交 | 2024-12-15 |
| 最新版本 | v2024.12.15.1 |
| License | **GPL-3.0** |
| 语言 | Shell，约 89 KB，266 stars |
| 用途 | 动态 swappiness 的 zRAM swap 管理器（Android + GNU/Linux） |

**只研究 zRAM 管理机制，不采纳其策略**：

- 它解决的是"如何创建/调整 zram 设备、设置 swappiness、按压缩率动态调参"；
- 在 Android 17 + `mmd.zram.enabled=true` 的设备上，这套逻辑与 MMD 直接冲突（MMD 接管 `swapon_all` 的 zram 配置、写回设备、重压缩算法）；
- 本项目因此**只读** `/sys/block/zram*/mm_stat`，用于计算压缩比与"实际占用"，并把结果作为内存压力的辅助信号，不写任何 zram 节点。

**可借鉴**：`mm_stat` 字段顺序与压缩比计算方式（`orig_data_size / compr_data_size`），本项目按同一口径实现并在单测中固定了字段布局。

---

## 5. 官方资料要点

### 5.1 MMD（Android 17+）

来源：https://source.android.com/docs/core/perf/mmd

- Android **17 及以上**才支持 `mmd`；`sys.boot_completed=1` 后 `mmd_setup` 配置 zRAM，然后启用 `mmd` 服务。
- 职责：zRAM 配置、**重压缩**（idle 页 lz4 → zstd）、**写回**（idle 页 → `/data` 上的 loop 后备设备）、**每进程写回/预取**（`CachedAppOptimizer` 经 Binder 调用）。
- 关键属性（前缀 `mmd.`）：`mmd.zram.enabled`（默认 false）、`mmd.zram.size`（默认 `50%`）、`mmd.zram.comp_algorithm`、`mmd.zram.writeback.enabled`、`mmd.zram.recompression.enabled` 等；维护调度在 `system_server` 侧，前缀 `mm.`（`mm.zram.maintenance.*`）。
- 启用 `mmd.zram.enabled` 后，`swapon_all` 的 zram 配置与 `config_zramWriteback` **变为无效**。
- 内核依赖：`CONFIG_ZRAM_TRACK_ENTRY_ACTIME`（GKI 6.18+ 默认开启）或 `CONFIG_ZRAM_MEMORY_TRACKING`；缺失时回退到软件 idle 标记。

**对本项目的影响**：MMD 存在时，zRAM 相关的一切都归它管。NeuroSched 只读取 `mm_stat` 做观测，绝不写 zram 节点、不调 swappiness、不动 LMKD。这也意味着"内存优化"在本项目里的正确形态是**降低前台进程的无谓 CPU 需求**，而不是"扩大 swap"。

### 5.2 uclamp

来源：https://docs.kernel.org/scheduler/sched-util-clamp.html

- 取值范围分两套：**per-task / sysctl 是 0..1024 的"性能点"**；**cgroup 接口 `cpu.uclamp.min|max` 是 0..100 的百分比**（可写 `max` 代表 100）。
- 语义：`uclamp.min` 是下限（boost，可克服 PELT 爬升延迟、在异构核上偏向大核）；`uclamp.max` 是上限（cap，可把后台任务压到小核、压低频率）。
- 系统级：`sched_util_clamp_min` / `sched_util_clamp_max`（默认均 1024），另有 `sched_util_clamp_min_rt_default`。
- 聚合：rq 上 `uclamp[MIN]` 取所有任务的最大值，`uclamp[MAX]` 也取最大值 —— **只要同一 rq 上有未受限任务（max=1024），对同 rq 其它任务的封顶就会失效**。
- 副作用：过度封顶可能破坏 PELT 信号，导致 CPU 无空闲、`util_avg` 冲到 1024 后出现频率尖峰。

**对本项目的影响（已落实到代码）**：

1. `actuator/uclamp.rs` **实测尺度**：读取节点当前值，>100 判为 0..1024，否则判为 0..100；写入时按尺度换算（`128 → 13%`，`640 → 63%`）。不换算直接写 640 会被内核以 EINVAL 拒绝。
2. 只操作 **top-app cgroup**，不动系统级 sysctl（`enable_sched_util_clamp` 默认 false，属 opt-in）。
3. 因为 rq 聚合是取最大值，**对 top-app 单独设 `uclamp.max` 并不能封住整机频率**；它只能约束 top-app 自己的请求。文档与 UI 都明确写了这一点，不做过度承诺。
4. 温度高时只降 `uclamp.min` 与 `uclamp.max`，不写频率表。

### 5.3 PSI

来源：https://docs.kernel.org/accounting/psi.html

- `/proc/pressure/{cpu,memory,io}`，每行 `some`/`full`，字段 `avg10/avg60/avg300/total`（total 单位 µs）。
- `some` = 至少一个任务因该资源停顿；`full` = 所有任务都停顿（内存回收场景下才有意义）。
- 需要内核 `CONFIG_PSI=y` 且 `psi=1` 启动参数。

**对本项目的影响**：PSI 是内存压力的主判据，但必须**同时**看 `full` 与 swap-in 速率才能区分"正常换页"与"thrashing"。`collector/memory.rs` 用 `pswpin` 差分算 KiB/s，`policy/memory_guard.rs` 要求 `swap_in_rate > 4 MiB/s 且 full10 > 1.0` 才判 thrashing。

### 5.4 其他

- **Thermal mitigation**（https://source.android.com/docs/core/power/thermal-mitigation）：HAL 分级 `NONE/LIGHT/MODERATE/SEVERE/CRITICAL/EMERGENCY`。本项目自建分级（不读 HAL），并在文档中说明"derived"来源，避免声称与 HAL 一致。
- **Performance hint API**（https://source.android.com/docs/core/perf/performance-hint-api）：ADPF 需要应用主动接入；本项目作为系统侧模块无法替应用发 hint，故不采用。
- **KernelSU 模块指南**（https://kernelsu.org/guide/module.html）：`module.prop` 必需字段、`customize.sh` 安装期变量（`$MODPATH`/`$ARCH`/`$API`）、`service.sh` 开机脚本、`webroot/` 提供 WebUI、`action.sh` 提供动作按钮。本项目按此实现，并额外提供 `boot-completed.sh`。

---

## 6. 复用决策汇总

| 能力 | 决策 | 理由 |
|---|---|---|
| KernelSU 模块骨架 | 自写（借鉴 KernelSU-Tuner 的生命周期约定） | 约定是公开的，实现简单，直接写更可控 |
| 参数备份/回滚 | 自写（JSONL 事务 + 校验 + 所有权） | 参考项目的备份机制无法检测"外部覆盖" |
| PSI / procfs 解析 | 自写（零依赖 Rust） | 需要 `full` 与差分速率，参考实现不够 |
| 应用事件 | 自写（logcat events + companion 文件 + proc 兜底） | Adaptive-Performance 无许可；且需要多源降级 |
| zRAM 管理 | **不复用**，交给 MMD | 官方机制优先于自研 |
| LMKD / swappiness / vm.* | **不实现** | 与系统冲突，收益不可证 |
| WebUI | 自写（3 个静态文件 + 内嵌） | 需要"无 CDN、无框架、不能执行 shell" |

---

## 7. 许可合规

- 本项目：**GPL-3.0-or-later**。选择 GPL 而非 MIT，是因为生态内主要参考项目（KernelSU-Tuner、zram-swap-manager）为 GPL-3.0，避免将来任何"借鉴片段"引发许可冲突；同时与 KernelSU 模块生态惯例一致。
- 未复制任何第三方源码。文中引用的结构、字段名、属性名属于接口事实，不受版权保护。
- `Adaptive-Performance` 无许可 → 零参考，且在此明确记录，供后续维护者避免误用。
- 若未来引入 MIT 代码（如 Thrawl 的片段），需在文件头保留其版权声明；当前无此类文件。

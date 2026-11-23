# 架构设计

2026-11-07 实现补充。Runtime 的规则 Decision 进入 Actuator，Predictor 输出另经 `policy::prediction::preview` 形成只读未来目标建议。预览具有独立类型和输出字段，`writes_allowed=false`。当前生产根写入硬门为 false。Runtime 使用内核文件锁，查询 CLI 使用独立只读采集路径。双时钟、事务结算、统计版本和验证结果见 [本轮交付](IMPLEMENTATION_2026-11-07.md)。下图保留模块关系，预测至策略的具体路径以此说明为准。

## 1. 总览

```
┌──────────────────────────────────────────────────────────────────┐
│ KernelSU module (module/)                                        │
│   module.prop   customize.sh   service.sh   boot-completed.sh    │
│   uninstall.sh  action.sh      webroot/index.html                │
└───────────────┬──────────────────────────────────────────────────┘
                │ 启动 / 停止 / 卸载回滚
┌───────────────▼──────────────────────────────────────────────────┐
│ neuroschedd (Rust, 单进程, 锁定依赖, 静态 aarch64 ELF)           │
│                                                                  │
│  collector ──► predictor ──► policy ──► actuator ──► journal     │
│      │            │            │           │            │        │
│      │            │            │           └─ safety::ownership  │
│      │            │            └─ thermal_guard / memory_guard   │
│      │            └─ markov / temporal / confidence              │
│      └─ cpu / memory / pressure / thermal / battery / process    │
│                     / appevent                                   │
│                                                                  │
│  storage (logger, telemetry CSV, history ring)                   │
│  api (127.0.0.1 HTTP + 内嵌 WebUI)                                │
└──────────────────────────────────────────────────────────────────┘
```

设计原则（按优先级）：

1. **不写就没错**：默认 `observe_only`，任何写入必须显式开启。
2. **可解释**：每个状态迁移都带 `reason`，每次写入都带 `old → new → 校验结果`。
3. **可回滚**：写入前落盘事务日志，崩溃后反向重放。
4. **可测试**：所有 `/proc`、`/sys` 访问都经过 `SysRoot`，可注入 fixture 与故障。
5. **不重复系统**：MMD 管 zRAM、LMKD 管杀进程、HAL 管温控 —— 本项目只观测它们。

---

## 2. 关键抽象

### 2.1 `SysRoot`（`sys.rs`）

```rust
SysRoot::system()            // 根在 "/"
SysRoot::at(fixture_dir)     // 根在测试目录，"/proc/x" -> "<fixture>/proc/x"
    .with_faults(map)        // 注入 Unreadable / Empty / WriteFail / WriteSilentlyIgnored
    .read_only()             // 拒绝一切写入（SAFE_MODE 与 --self-test 使用）
```

- 所有路径都是**绝对路径字符串**，`SysRoot` 负责重定基。这样同一份采集代码既能在手机上跑，也能在 CI 里跑。
- `recorded_writes()` 记录本次进程写过的每一个 `(path, value)`，测试用它证明"observe 模式一个字节都没写"。
- `Fault::WriteSilentlyIgnored` 模拟"内核接受写入但读回旧值"（例如被 clamp），用于验证回读校验真的生效。

### 2.2 `Snapshot`（`types.rs`）

一次采样产生一个不可变快照，包含 CPU / 内存 / PSI / 温度 / 电池 / 前台应用 / 采集器健康。下游（预测器、策略、API）只读快照，不再回读 sysfs —— 保证同一 tick 内所有决策看到的是**同一时刻**的数据。

### 2.3 采样节拍

| 数据 | 频率 | 实现 |
|---|---|---|
| CPU 负载 / 频率 | 1 s（息屏 5 s） | `/proc/stat` 差分 + `scaling_cur_freq` |
| 内存 / zRAM / PSI | 1 s | `/proc/meminfo`、`/proc/vmstat`、`/proc/pressure/*`、`mm_stat` |
| 温度 | 4 s（内部缓存） | `thermal_zone*/temp` |
| 电池 | 1 s | `power_supply/*` |
| 前台应用 | 事件驱动 | `logcat -b events` 长驻子进程 |
| 历史聚合 | 60 s | 内存环形 720 点 + CSV 落盘 |
| 预测 | 5 s | 避免每 tick 重算分布 |
| 写入复核 | 状态变化时 + 每 60 s | 防止被外部静默改回 |

**没有**任何"每秒 fork 一个 shell"的路径。唯一的子进程是 `logcat`（长驻，一次）与启动时一次的 `getprop`。

---

## 3. 采集层

| 模块 | 关键实现细节 |
|---|---|
| `cpu.rs` | `/proc/stat` 差分算负载；簇由 `related_cpus` 分组，按 `cpuinfo_max_freq` 降序命名 `prime/gold/silver/bronze`；同时探测 WALT、uclamp、`sched_util_clamp_*` 是否存在（只读） |
| `memory.rs` | `MemAvailable`、swap、`pswpin/pswpout` **差分算 KiB/s**、`pgscan_kswapd`、zRAM `mm_stat`（按固定字段序解析：orig/compr/used/limit/…） |
| `pressure.rs` | 解析 `some`/`full` 的 avg10/60/300/total；缺失时 `available=false`，不伪造 0 |
| `thermal.rs` | 按 `type` 分类 zone；`cpu_c` 只取 **CPU 类** zone 的最大值；毫度/度自适应；4 s 缓存 |
| `battery.rs` | `power_supply` 中 `type=Battery` 的那一个；温度按"十分之一度"处理 |
| `process.rs` | `kswapd0` 的 utime+stime 差分（USER_HZ=100）；MMD 探测 |
| `appevent.rs` | 三级降级：logcat events → companion 文件（按大小 tail）→ `/proc` top-app 扫描（低置信度） |

### 前台应用识别为什么不用 `dumpsys`

`dumpsys activity activities` 单次约 30–80 ms CPU、且需要 binder 往返。1 Hz 轮询等于持续唤醒 `system_server`，与本项目目标（降低无谓唤醒）直接矛盾。因此：

1. **首选** `logcat -b events -s wm_on_resume_called:I wm_on_top_resumed_gained_called:I am_proc_start:I screen_toggled:I` —— 一个长驻子进程，事件到达即处理，零轮询；
2. **次选** companion 写入的 `fg_events.log`（按文件大小增量读取，1.5 s 检查一次 `stat`）；
3. **兜底** `/proc/*/cgroup` 中含 `top-app` 且 `oom_score_adj` 最小的进程，5 s 一次，且**置信度上限 0.5**，预测器会因此更容易拒绝预测。

---

## 4. 预测层

```
        ┌──────────────┐
事件 ──► │ MarkovChain  │  一阶转移，P(next|current)
        ├──────────────┤
        │ TemporalProfile │ 星期×小时桶（168 桶），P(pkg|bucket)
        ├──────────────┤
        │ Ewma(interval) │  启动间隔，衡量"该来了吗"
        └──────┬───────┘
               ▼
   score = 0.50·markov + 0.35·temporal + 0.15·recency
               ▼
   confidence = (0.45·coverage + 0.35·margin + 0.20·certainty) × source_trust
               ▼
   若 events < min_samples 或 confidence < min_confidence → refused
```

- `coverage = min(1, events/min_samples)`，`margin = (p1-p2)/p1`，`certainty = 1 - H/log2(n)`。
- `source_trust`：logcat / companion = 1.0，`/proc` 扫描 = 0.5。
- 命中判定：预测后 120 s 内实际前台变成该包 → `hits += 1`；准确率在 UI 展示。
- **不启动应用**，不加载模型，不联网。模型每 5 min 落盘一次（`model.json`）。

---

## 5. 策略层

### 5.1 状态与优先级

| 优先级 | 状态 | 进入条件（摘要） | 配置键 |
|---|---|---|---|
| 100 | `SAFE_MODE` | 看门狗判定崩溃循环 / 手动 | `safety.*` |
| 90 | `THERMAL_GUARD` | CPU 类最高温 ≥ `enter_moderate_c`，退出需 ≤ `enter_light_c - margin` | `policy.thermal.*` |
| 80 | `MEMORY_PRESSURE` | PSI some/full、`MemAvailable%`、`kswapd0%` 任一越线 | `policy.memory.*` |
| 70 | `BATTERY_SAVER` | 电量 ≤ `saver_pct` 且（默认）未充电 | `policy.battery.*` |
| 60 | `APP_LAUNCH` | 前台变化后 `boost_ms` 内 | `policy.app_launch.boost_ms` |
| 50 | `GAME` | 前台包名 ∈ `policy.game.packages` | 用户配置 |
| 40 | `FOREGROUND` | 有前台应用、非桌面、屏幕亮 | — |
| 30 | `BALANCED` | 兜底（不参与竞争，列表为空时使用） | — |
| 20 | `IDLE` | 屏幕关闭 | `policy.idle.*` |
| 10 | `BOOT` | 启动后 `boot_grace_ms` 内 | `policy.boot_grace_ms` |

`BALANCED` 是**兜底**而不是竞争者：否则它的 30 分会压过 `IDLE`(20)，屏幕关闭后永远进不了省电态。这是对需求文档优先级表的一处有意偏离，已在 `policy/engine.rs` 注释说明。

### 5.2 防抖四件套

| 机制 | 作用 |
|---|---|
| 进入阈值 / 退出阈值 | 例如 PSI some 进入 12、退出 4 |
| 最短驻留 `min_dwell` | 离开当前状态前必须待够（保护态豁免） |
| 冷却 `cooldown_ms` | 降级后 8 s 内不再降级，专门治 2 Hz 抖动 |
| 保护态抢占 | `priority ≥ MEMORY_PRESSURE` 的状态可无视驻留立即进入 |

### 5.3 否决链

```
预测器 ──(仅建议)──► 策略 ──► 温度权限 ──► 内存压力 ──► SAFE_MODE ──► 执行器
                                  │            │           │
                                  └─ 只能降低 uclamp_max / 清零 uclamp_min ─┘
```

- 温度 `permission`：`allow → trim → no_boost → cap`，对应 `uclamp_max` 上限 `1024/960/800/640`。
- 温度非 `allow` 时，任何状态都不得保留 `uclamp.min > 0`（记录在 `blocked`）。
- 内存压力时同样清零 `uclamp.min` —— 对应需求"内存压力高时不应继续提高全局 CPU 负载"。
- `SAFE_MODE` 直接使用 `Profile::default()`（0/1024），等价于"不干预"。

---

## 6. 执行层

### 6.1 允许写的节点（v1 全部）

| 节点 | 条件 |
|---|---|
| `<top-app cgroup>/cpu.uclamp.min` | `actuator.enable_uclamp_top_app=true`（默认 true）且节点存在 |
| `<top-app cgroup>/cpu.uclamp.max` | 同上 |
| `/proc/sys/kernel/sched_util_clamp_min` | `actuator.enable_sched_util_clamp=true`（**默认 false**，全局影响） |

其余一切节点被 `actuator/cpufreq.rs::forbidden()` 硬拦截，包括：`/sys/devices/system/cpu/*`（频率/online/affinity）、`/sys/class/thermal/*`、`/sys/block/zram*`、`/proc/sys/vm/*`、LMKD/MMD 模块参数、`/dev/stune`、debugfs、`/sys/class/kgsl`、`/sys/power`。

### 6.2 写入八步

```
1 read current ──► 2 ownership check ──► 3 validate range ──► 4 journal(pending)
                                                                     │
8 restore on failure ◄── 7 verify readback ◄── 6 read back ◄── 5 write
```

- 第 2 步：若该节点已被释放（外部覆盖），直接跳过并记 `released`。
- 第 4 步：`fsync` 后才继续 —— 崩溃时"我改了什么"必须已经在磁盘上。
- 第 7 步：读回值 ≠ 目标值 → 立即写回旧值，记 `verify_failed`，状态 `failed`。
- 第 8 步：写失败同样先尝试恢复旧值，再记录。

### 6.3 uclamp 尺度

`cpu.uclamp.*` 的 cgroup 接口是 **0..100 百分比**，而 sysctl / per-task 是 **0..1024**。执行器读取节点当前值判定尺度（任一 >100 即 0..1024），写入时换算：

```
profile 128 → 13%（percent 节点）   /  128（perf 节点）
profile 640 → 63%                   /  640
profile 1024 → 100%                 /  1024
```

不换算直接写 640 会被内核 EINVAL 拒绝；换算后即使尺度判断错误，也会被第 7 步读回校验捕获并回滚。

---

## 7. 存储布局

```
/data/adb/neurosched/            0700
├── config.json                  配置（原子写：tmp + rename）
├── token                        WebUI/API 令牌（32 hex，0600）
├── model.json                   预测模型
├── boot.json                    看门狗状态（启动次数/失败次数/SAFE_MODE 余额）
├── daemon.pid                   守护进程 pid
├── stop                         协作停止标记（由 --stop 创建）
├── fg_events.log                companion 应用写入的前台事件（可选）
├── logs/neurosched.log          运行日志（>512 KiB 轮转）
├── logs/capability.json         boot-completed 生成的能力报告
├── logs/daemon.out              service.sh 捕获的 stdout/stderr
├── telemetry/history.csv        60 s 聚合，8 MiB 轮转，保留 8 份
├── journal/<session>.jsonl      写入事务日志
└── backups/                     预留（导出/导入用）
```

---

## 8. API

默认监听 `127.0.0.1`（`Config::validate` 把非回环地址改回），所有 `/api/*` 使用 `X-NeuroSched-Token` 或 Bearer Header 认证。Host 必须为合法回环 authority，浏览器 Origin 存在时必须同源。token query 被拒绝。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/` `/app.js` `/style.css` | 内嵌 WebUI，人工输入凭证 |
| GET | `/api/status` | 完整状态文档（Dashboard 一次拉取） |
| GET | `/api/policy` | 决策 + 迁移历史 + 阈值配置 |
| GET | `/api/cpu` `/api/memory` | 分页数据 |
| GET | `/api/predict` | 预测 + 准确率 + 使用排行 |
| GET | `/api/safety` | 模式、节点所有权、冲突、事务日志 |
| GET | `/api/logs?limit=` | 日志环形缓冲 |
| GET | `/api/history?limit=` | 历史点 |
| GET | `/api/capability` | 能力探测结果 |
| GET | `/api/export.csv` | 导出 telemetry |
| POST | `/api/config` | 局部更新配置（白名单字段，`validate()` 后落盘） |
| POST | `/api/action` | `mode` / `rollback` / `reset_model` / `predict` / `reload` |

**没有**执行 shell 的端点。`/api/action` 只接受上面 5 个固定字符串。

---

## 9. 关键取舍

| 取舍 | 选择 | 理由 |
|---|---|---|
| Rust 依赖 | **零 crate** | 模块体积可控（783 KiB 静态二进制）、无供应链风险、无 NDK 依赖（musl + `rust-lld` 自包含链接） |
| HTTP 服务 | 手写 200 行 | 只需 12 个端点；引入 axum/tokio 会让二进制膨胀 3–5 倍 |
| JSON | 手写 parser/serializer | 只服务配置文件与 API，200 行足够，且能被 fixture 单测覆盖 |
| 应用事件 | 三级降级 | logcat 最准但可能被裁剪；`/proc` 兜底但不可靠，所以用 `source_trust` 表达不确定性 |
| uclamp 只写 top-app | 是 | 全局 sysctl 影响所有任务，风险大；top-app 是收益/风险比最高的一层 |
| `BALANCED` 兜底 | 是 | 见 5.1 |
| 不写频率表 | 是 | 与厂商 governor / HyperOS 调度器直接冲突，且无法安全回滚 |
| 温度分级自算 | 是 | 读 `dumpsys thermalservice` 需 binder + shell，1 Hz 不可接受；自算分级在 UI 标注 `derived` |

---

## 10. 失败模式

| 失败 | 表现 | 兜底 |
|---|---|---|
| 节点不存在 | `capability.uclamp = null` | 执行器空转，模式仍可切换，`/api/status` 明确显示 |
| 节点不可写 | 写入返回 EACCES | 立即回滚，`report.failed` 计数，日志记录 |
| 内核 clamp 值 | 读回 ≠ 目标 | 恢复旧值，标记 `failed` |
| 外部服务抢写 | 读回 ≠ 我们最后写的值 | 释放节点，本次会话不再写 |
| 守护进程被 kill -9 | 事务日志未闭合 | 下次启动反向重放，恢复旧值 |
| 崩溃循环 | 连续 3 次未到 healthy | SAFE_MODE（N 次启动内只读），健康一次后递减 |
| 配置损坏 | JSON 解析失败 | 移到 `config.bad.<ts>.json`，加载默认值并告警 |
| 温度传感器缺失 | `cpu_c = null` | 热保护不触发（安全侧失败），UI 明确标注 |
| 屏幕状态未知 | `screen = unknown` | IDLE 不触发，保守停在当前状态 |

# 安全与回滚

本文件说明 NeuroSched 在什么条件下会写、写什么、怎么写、写坏了怎么回来。

2026-11-07 安全边界。当前生产构建 `ACTUATOR_ENABLED=false`，`SysRoot::write` 对真实根拒绝所有内核写入，恢复也受此门控制。下面的执行事务经合成树验证，设备写入资格和异常恢复演练仍须单独批准。只读观测可生成计划及未来应用模拟建议。

v0.1.1-observe.1补充：Runtime及CLI恢复均先校验当前内核boot_id、active标记与session身份。同boot才可进入回放；跨boot只持久归档旧证据、不回放，并在本次Runtime保持安全观察。缺失或损坏的身份及非空旧日志会拒绝恢复并保留现场；仅零字节旧日志可无写迁移。新Runtime缺少合法boot_id时拒绝启动。

运行主循环另有不依赖共享Mutex的Atomic活性期限，正常采样等待计入期限；任务预算30秒，检查间隔1–10秒。失活时只结束自身进程(exit70)，保留日志，由现有模块守护处理；不重启设备。健康标记要求当前CPU/内存/温度质量与时效。HTTP采用8连接、5秒固定I/O期限和固定worker；应用锁/文件I/O不受socket期限强制取消。新边界与验证限制见 [优化交付](OPTIMIZATION_2026-11-07.md)。

## 1. 红线（代码层面强制，不是文档约定）

| 禁止项 | 实现位置 |
|---|---|
| 修改 CPU 频率 / online / offline / affinity | `actuator/cpufreq.rs::forbidden()` 前缀拦截 `/sys/devices/system/cpu/` |
| 修改 GPU 频率 | 同上，拦截 `/sys/class/kgsl` |
| 修改温控阈值 / trip point / 关闭温控 | 同上，拦截 `/sys/class/thermal`、`/sys/class/hwmon` |
| 修改 zRAM 大小 / 算法 / 开关 | 同上，拦截 `/sys/block/zram` |
| 修改 swappiness / min_free_kbytes / dirty_ratio | 同上，拦截 `/proc/sys/vm` |
| 修改 LMKD / MMD | 同上，拦截 `/sys/module/lowmemorykiller`、`/sys/module/mmd` |
| 修改 schedtune / debugfs / power | 同上，`/dev/stune`、`/sys/kernel/debug`、`/sys/power` |
| 全局调度器调参 | 当前计划与恢复均不支持，只读展示已有值 |
| 关闭 SELinux | 无任何相关代码 |
| `chmod 777` | 无任何 `chmod` 调用；目录用 `0700`，token 用 `0600` |
| 刷写分区 / 重启设备 | 无任何 `reboot`、`dd`、`fastboot` 调用 |
| 从 WebUI 执行 shell | API 无 shell 端点，`/api/action` 只接受 5 个固定动作 |
| 联网 | 无任何网络客户端代码；API 仅监听 `127.0.0.1` |

计划经过 `forbidden()`、范围和所有权检查。恢复采用精确 cgroup 路径白名单。两条路径最终均进入 `SysRoot::write` 的生产硬门，fixture 另受规范路径边界约束。

---

## 2. 默认状态

```json
{ "mode": "observe_only", "actuator": { "enabled": false, "enable_uclamp_top_app": true, "enable_sched_util_clamp": false } }
```

- `observe_only`：采集、预测、决策全部运行，**写入路径完全不执行**（`RunMode::writes_allowed() == false`）。
- `dry_run`：同样不写，但报告里给出逐节点 `old → new` 与"为什么"。
- `enabled` 保留兼容解析和 fixture 验证。API 拒绝开启，生产硬门强制只读，缺少独立 consent 时也降级。

安装包里的 `config.default.json` 由守护进程自身生成，打包脚本会校验 `mode == "observe_only"`，不满足则拒绝出包。

---

## 3. 写入流程

### 3.1 八步事务

```
1. 读取当前值           read_trim(node)
2. 所有权检查           released? → skip；last_written != current? → conflict
3. 范围校验             0..=scale.max()，且 uclamp.min ≤ uclamp.max
4. 事务日志（pending）   fsync 到 journal/<session>.jsonl
5. 写入                 write(node, value)
6. 回读                 read_trim(node)
7. 校验                 不等 → 写回 old，记 verify_failed
8. 失败恢复             写失败同样写回 old，记 write_error
```

关键点：**第 4 步在第 5 步之前**。进程在任意时刻被 `kill -9`，磁盘上都已经有"我要把 A 改成 B"的记录，下次启动可以反向重放。

### 3.2 允许的两个节点

| 节点 | 默认 | 影响面 | 风险 |
|---|---|---|---|
| `<top-app>/cpu.uclamp.min` | 启用 | 仅 top-app cgroup 内的任务 | 低：只影响前台应用的性能下限 |
| `<top-app>/cpu.uclamp.max` | 启用 | 同上 | 低：同上；注意 rq 聚合取最大值，同 rq 有未受限任务时封顶会失效 |
| `/proc/sys/kernel/sched_util_clamp_min` | 当前不支持 | 全系统所有任务 | 配置字段仅保留兼容与诊断 |

### 3.3 uclamp 接口单位

cgroup ABI 使用 `0..100` 有理百分比，`max` 解析为100。内部 profile 使用 `0..1024`，计划换算到两位小数，128对应12.50。挂载信息必须确认 CPU cgroup，未知尺度不生成写计划。挂载匹配、读取成功和权限位仍不足以证明 SELinux 或厂商共存允许写入。

---

## 4. 冲突处理（与 Scene 等共存）

```
当前值 == 我们最后写入的值 ? 继续 : 冲突
```

当前冲突处理固定采用 `release`。

1. 该节点标记 `released`，写入本次会话永久跳过；
2. `report.conflicts` 记录 `ours=…, now=…`；
3. WebUI Safety 页显示"已释放 + 原因"；
4. **不覆盖对方的写入**，卸载/回滚时也不碰它。

测试：`external_writer_wins_and_node_is_released`（含 shutdown 后仍不覆盖）。

`on_conflict="ignore"` 保留配置兼容，当前仍主动释放。成功恢复或释放后持久化 `settled`，立即恢复成功的失败尝试记 `cancelled`。结算持久化失败时禁写并保留 active。重放归并 pending/完成记录，冲突后停止该节点的更旧记录。

值比较和实际写入之间存在竞争窗口，同值 ABA 外部写入也无法凭读值识别。厂商服务共存仍需实机证据。

---

## 5. 崩溃与异常

| 场景 | 机制 | 结果 |
|---|---|---|
| 守护进程被 kill -9 | `journal/active` 未删除 + 该 session 的条目 | 下次启动反向重放，只在"当前值 == 我们写的值"时恢复 |
| 写入过程中断电 | 同上 | 同上 |
| 内核 clamp 值 | 回读校验 | 立即恢复旧值，`report.failed` |
| 配置损坏 | `Config::load_or_recover` | 移到 `config.bad.<ts>.json`，加载默认值并告警 |
| 启动即崩（连续 3 次） | `Watchdog` | 进入 `SAFE_MODE`（默认 3 次启动内只读），健康运行 90 s 后递减 |
| 采样主线程卡死 | 当前同线程 watchdog 无法自行执行 | 独立活性检测尚待实现 |
| 采样线程 panic | `panic = "abort"`，进程退出 | supervisor 5–60 s 退避重启；真实恢复能力需实机验收 |

`SAFE_MODE` 下：`Actuator` 被强制为 `observe_only`，`Engine` 使用 `Profile::default()`，启动时先执行一次 `restore_all()`。

---

## 6. 回滚路径（从轻到重）

| 方式 | 操作 | 适用 |
|---|---|---|
| 1. WebUI 按钮 | Safety 页 → "立即回滚全部节点" | 日常 |
| 2. 命令行 | `neuroschedd --data-dir /data/adb/neurosched --restore-all` | 要求运行锁可取得；当前生产硬门下恢复写入返回失败并保留证据 |
| 3. 切换模式 | WebUI 改 `observe_only` 或 `neuroschedd --stop` | 只想停写 |
| 4. KernelSU 禁用模块 | 模块卡片 → 禁用 → 重启 | 守护进程完全不启动 |
| 5. 卸载模块 | 模块卡片 → 卸载 | 核实PID身份、协作停止、独立恢复，数据与日志保留 |
| 6. 人工恢复方案 | 核对日志、挂载和实时节点，提出明确恢复方案 | 设备写入需用户批准 |

`--restore-all` 与 `uninstall.sh` 都遵守同一条规则：**只在当前值仍等于我们写入的值时恢复**，不覆盖第三方的新值。

---

## 7. 安装与卸载

### 安装

- 只创建 `/data/adb/neurosched/{logs,telemetry,journal,backups}`，权限 `0700`；
- 只在 `config.json` **不存在**时写入默认配置（升级不覆盖用户阈值）；
- 安装脚本会执行一次 `neuroschedd --version` 验证二进制可运行，失败则明确提示"模块不会启动"；
- **不重启、不刷写、不修改现有 Root 环境**。

### 卸载

`uninstall.sh` 核实 `/proc/PID/exe` 后协作停止，最多等待30秒。超时、身份变化、stale PID伴随active或恢复失败均返回非零并保留全部数据。升级保留已有配置，stop标记跨重启保留。KernelSU各版本如何处理卸载脚本非零退出仍需实机确认。

**不会**删除 Scene、不会卸载其他模块、不会改动 `/data/adb/modules` 以外的任何路径。

---

## 8. 已知风险

| 风险 | 可能性 | 影响 | 缓解 |
|---|---|---|---|
| 与 HyperOS 自带调度服务竞争 | 未实测 | 状态覆盖、时序竞争或性能退化 | 当前生产硬门关闭，后续核实真实写入者与控制边界 |
| 厂商 uclamp ABI 与标准不同 | 未实测 | 不兼容的计划或恢复 | 挂载验证、未知尺度拒绝、回读校验 |
| `uclamp.max` 封顶因 rq 聚合失效 | 高（内核语义） | 封顶无效，等于没做 | 文档明示；不宣称"降低整机频率" |
| 温度 zone 无法映射 | 未实测 | CPU温度未知 | Fail-closed至observe_only，self-test报告映射和信任 |
| 封顶或Boost效果不符合预期 | 未实测 | 交互变慢、温升或能耗增加 | 受控A/B和长期基线，收益证据不足时保持只读 |
| `logcat -b events` 被裁剪 | 未实测 | 前台识别退化 | 来源信任与失活过期，低信任拒绝预测 |
| 静态 musl 二进制无法在目标加载 | 未实测 | 服务启动失败 | 目标ABI、ELF加载和系统调用兼容性逐项验收 |

---

## 9. 安全相关的设计取舍

1. **宁可少做**：v1 只写 top-app 的 uclamp。收益有限，但失败面也有限。
2. **宁可报错**：任何不确定（尺度、范围、所有权）都选择"不写"，而不是"猜一个值"。
3. **不隐藏失败**：`blocked`、`conflicts`、`released`、`failed` 全部出现在 API 与 UI 上，不做"看起来一切正常"的假象。
4. **不使用 root 特权做危险事**：有 root 不等于应该改分区、关 SELinux、改温控。

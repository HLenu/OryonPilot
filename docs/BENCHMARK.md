# 性能基准方法论

> **状态：无数据。** 设备未连接，本文件只定义"怎么测"与"什么算成功"，不含任何测量结果。
> 任何没有 `docs/audit-*.txt` 与 `bench/*.csv` 支撑的"优化了 X%"都应被视为编造。

---

## 1. 原则

1. **先基线，后干预**：先跑 24–48 h `observe_only`，再开 `dry_run`，最后才做 `enabled` 的 A/B。
2. **同一台机、同一充电状态、同一室温、同一批应用**，每次测量前清空后台并等 10 min 热平衡。
3. **不用跑分证明**：安兔兔/GeekBench 与"日常流畅、待机省电"没有稳定相关性。
4. **先看有没有害，再看有没有益**：任何指标恶化超过噪声即回退。
5. **无法证明收益 → 关闭执行，保留监控**（这是项目的既定规则）。

---

## 2. 指标与采集方式

| # | 指标 | 采集方式 | 备注 |
|---|---|---|---|
| 1 | 冷启动延迟 | `am start -W -S <pkg>/<act>` 的 `TotalTime`，重复 10 次取 P50/P95 | 每次前 `am force-stop` |
| 2 | 热启动延迟 | `am start -W`（不 `-S`），重复 15 次 | 同一会话内 |
| 3 | 应用切换延迟 | `logcat -b events` 的 `wm_on_resume_called` 时间差（A→B 的 resume 间隔） | 手工脚本切换 |
| 4 | 桌面交互流畅度 | `dumpsys gfxinfo com.miui.home framestats` 的 P50/P90/P99 帧时长 | 固定滑动脚本 |
| 5 | 游戏持续负载 | 30 min 固定场景，记录 `fps`（游戏内）+ 频率 + 温度 | 同一画质设置 |
| 6 | 10 min+ 持续高负载 | `stress-ng --cpu 8`（若无则 8 个 `yes > /dev/null`） | 观察是否降频/降亮度 |
| 7 | 待机耗电 | 飞行模式+息屏 8 h，`dumpsys batterystats` 的 mA 均值 | 同一电量区间 |
| 8 | 内存压力 | PSI `some/full` avg10、`MemAvailable`、`kswapd0` CPU% | 本项目自带 CSV |
| 9 | kswapd0 活跃度 | `/proc/<kswapd0>/stat` 差分 | 本项目 1 Hz 采集 |
| 10 | 温度曲线 | `thermal_zone` 中 CPU 类 zone + battery | 本项目 4 s 采集 |

本项目自带的 `telemetry/history.csv`（60 s 聚合）可直接用于 5/6/7/8/9/10 的长期对比：

```bash
adb pull /data/adb/neurosched/telemetry/history.csv bench/
# 或在 WebUI Logs 页点"导出 CSV"
```

CSV 列：`ts_iso,state,cpu_pct,prime_mhz,mem_avail_mb,swap_used_mb,zram_ratio,psi_some10,psi_full10,kswapd_pct,cpu_temp_c,batt_temp_c,batt_pct,foreground`

---

## 3. 阶段设计

### 阶段 A：基线（24–48 h，`observe_only`）

```bash
# 安装模块 → 重启 → 不做任何手动优化
# 正常使用一天，第二天导出
adb pull /data/adb/neurosched/telemetry/history.csv bench/A-baseline.csv
adb pull /data/adb/neurosched/logs/capability.json bench/A-capability.json
```

产出：状态分布（各 state 占比）、温度分布、PSI 分布、前台应用序列。
**这一步同时验证"监控本身不引入可测开销"**：对比安装前后的待机电流与 `top` 中 `neuroschedd` 的 CPU%。

预期（需实测确认）：1 Hz 采样下守护进程自身 CPU 占用应 < 1%，常驻内存 < 20 MB。

### 阶段 B：Dry Run（24 h）

```bash
# WebUI → Safety → dry_run
```

产出：`report` 中出现多少次 `planned` 写入、集中在哪些状态、目标值是否合理。
**此阶段不产生任何实际改动**，用来检查策略是否"想说人话"。

### 阶段 C：受控 A/B（各 48 h）

- A 组：`observe_only`（对照）
- B 组：`enabled`
- 两组之间保持相同的应用使用脚本（或至少记录 usage 序列，事后按前台应用对齐）

对比项：P50/P95 启动延迟、帧时长 P99、待机 mA、CPU 温度均值/峰值、PSI full 均值。

---

## 4. 判定标准（预先写死，避免事后找理由）

| 结论 | 条件 |
|---|---|
| **保留执行** | 启动延迟 P95 不劣化 > 3%，且（温度峰值下降 ≥ 2 °C 或待机电流下降 ≥ 3%） |
| **仅保留监控** | 启动延迟 P95 劣化 > 3%，或温度/电流无改善 |
| **立即回退** | 出现任何卡顿报告、ANR 增加、或 `report.failed > 0` |

噪声处理：每组至少 10 次采样取 P50/P95，跨天重复 2 轮；差异小于组内标准差的视为无差异。

---

## 5. 预期与已知限制

**预期收益有限，原因如下（必须写清楚）**：

1. 本项目只写 top-app 的 uclamp，**不动频率表**。8 Elite 的 schedutil 本身响应很快，uclamp 的边际收益主要体现在"减少无谓的 prime 簇唤醒"。
2. `uclamp.max` 受 rq 聚合语义限制：只要同 rq 上有未受限任务，封顶就失效。因此"降低整机频率"不是本项目的目标，也不应作为验收标准。
3. MMD 已经接管 zRAM，LMKD 已经管杀进程，本项目在这两块**没有**优化空间，只有观测价值。
4. 真正的收益更可能出现在：待机时减少无效 boost（IDLE 态 `uclamp.min=0`）、热保护时更早收敛（避免温度冲到 SEVERE 再猛降）。

**因此本项目的诚实定位**：一套可解释、可回滚的资源观测与温和调控系统，而不是"性能提升 X%"的魔法模块。

---

## 6. 无法测量的项

| 项 | 原因 |
|---|---|
| 帧时间（可靠） | `gfxinfo` 在 HyperOS 上可能被裁剪；需要实机确认 |
| 应用内启动耗时 | 需要应用配合（ADPF/宏基准），系统侧测不到 |
| 真实用户感知 | 只能靠主观反馈，无法量化 |

这些项在本项目中**不作为验收依据**，只作为参考记录。

# 测试报告

## 2026-11-07 v0.1.1-observe.1 最终验证（当前）

**247库单测 + 8 CLI + 34集成 = 289项Rust全部通过**，最终完整`cargo test --manifest-path daemon/Cargo.toml --locked --offline --quiet` exit0。主机及ARM64 all-targets clippy `-D warnings`通过，fmt通过；Python构建回归24通过，`node --check webui/app.js`通过。ARM64 Unix测试只跨编译，不算实机运行。新增覆盖boot_id、独立失活退出、当前采集健康、stop打断长等待、HTTP真实TCP边界/并发配置、资源指标、脱敏与能力探测。命令及初次失败/修复过程见 [本轮优化交付](OPTIMIZATION_2026-11-07.md)。

本轮源码实际重建ARM64 musl release和主机程序，ELF静态/16KiB对齐、ZIP17条目及安全默认值校验通过。包588856字节，SHA256 `07746e2b10c64499b5294de622865eaec3d35541f7b7c258d5daece4c69d336d`。

目标小米15新版短时测试通过：5个模块脚本Android sh语法、自检0 failures、单次采集、隔离Runtime3帧API诊断、未鉴权401/鉴权200、正常停止、logcat退出、临时目录/转发清理。WALT=true，页大小4kB。自己的write journal为0字节。三帧当次7项采集器健康均ok，累计错误1且未增长；不据此宣称全程无异常。原始证据在本轮artifact根`01a1b64d-049a-74a1-a677-65cdb87f9bf8`。未安装新版，未测WebView交互/完整生命周期/长稳/性能/功耗/真实写入。

---

## 2026-11-07 小米15首次适配修复验证（历史）

在前次247项测试基础上新增4项回归：真实HyperOS Activity类名拒绝、系统Component事件跨页面去重、零时刻有效温度缓存、非温度BCL节点排除。4项均先在旧实现复现失败，再修复实现。当前完整Rust命令exit0：**213库单测+8CLI+30集成=251通过**，无失败。

环境和输出路径沿用下方前次说明；本次测试设置TEMP/TMP至artifact根，使用`CARGO_INCREMENTAL=0`、locked/offline，HTTP集成需主机回环端口权限。实际执行命令：

```powershell
# 修复前证据：2项、1项、1项分别失败
cargo test --manifest-path daemon/Cargo.toml --locked --offline --lib xiaomi -- --nocapture
cargo test --manifest-path daemon/Cargo.toml --locked --offline --lib sample_at_monotonic_zero -- --nocapture
cargo test --manifest-path daemon/Cargo.toml --locked --offline --lib battery_limit_virtual_sensors -- --nocapture

# 最终源码验证（全部通过）
cargo fmt --manifest-path daemon/Cargo.toml --all -- --check
cargo test --manifest-path daemon/Cargo.toml --locked --offline --quiet
cargo clippy --manifest-path daemon/Cargo.toml --locked --offline --all-targets -- -D warnings
$env:RUSTFLAGS = '-C linker=rust-lld -C link-arg=-z -C link-arg=max-page-size=16384 -C link-arg=-z -C link-arg=common-page-size=16384'
cargo clippy --manifest-path daemon/Cargo.toml --locked --offline --target aarch64-unknown-linux-musl --all-targets -- -D warnings
```

调用已有`scripts/build.py`的main（默认参数），仅将DIST指向新的`xiaomi15-readonly/build-output-verified`。脚本实际运行`cargo build --locked --offline --release --target aarch64-unknown-linux-musl`及主机`cargo build --locked --offline`；两者成功。ELF静态AArch64、约965KiB、PT_LOAD=16384，ZIP 11条目及observe_only/actuator.enabled=false/硬门false校验通过。首次打包调用曾因父进程PATH未包含Cargo而报WinError2；补齐PATH后成功，未改构建脚本。最终包哈希见DEVICE_CAPABILITY.md。

修复后Windows二进制分别运行`--fixture <captured-root> --data-dir <unused-state> --self-test`和同参数`--once`，两次exit0，未创建数据目录；温度72 zones、CPU45.9°C、trusted。原先`--once`在ts_ms=0会误报stale_data，现已消除。前台代码使用设备实际`wm_set_resumed_activity`记录测试，未在手机上启动Watcher。

真实ARM64执行另外验证：临时上传当前产物，比对SHA256后，以SukiSU Root执行`timeout 20 <临时二进制> --data-dir <临时目录>/state --self-test`及`--once`。两者exit0，自检root=/、verdict=ok、failures=0；两簇、72温度节点、CPU41.7°C、battery70%/Charging，执行硬门及writes_allowed均false。state目录未创建，临时文件删除并验证不存在。原始命令/退出码在`xiaomi15-readonly/device-run-evidence.json`；输出在`device-self-test.json`和`device-status.json`。

未测试uclamp写入、常驻Runtime/Watcher、模块安装/启动/升级/卸载、SukiSU WebView、性能、续航或长稳。Python24项/JS语法结果沿用前次证据（此次未修改对应实现、未重跑），不计入本次251项Rust测试。Unix专项测试仅跨编译与clippy；短时设备自检不等于运行Unix单测集。

---

## 2026-11-07 设备连接前验证

环境为Windows、Rust/Cargo 1.99.1、Python3.14.7、Node24.20.0。依赖使用现有Cargo.lock，命令为locked/offline。TEMP、构建输出和合成树位于独立artifact目录。

| 检查 | 实际结果 |
|---|---|
| fmt check、主机clippy all-targets -D warnings | 通过 |
| Rust库单测 | 209通过 |
| CLI单测 | 8通过 |
| simulated_device集成 | 30通过 |
| Python打包与ELF回归 | 24通过 |
| node --check webui/app.js | 通过 |
| ARM64 musl all-targets check/clippy | 通过，Unix测试仅跨编译 |
| ARM64 musl release | 通过，静态ELF、PT_LOAD 16384、964KiB |
| 候选ZIP | 569681字节，11条目，默认只读与硬门false校验通过 |
| Shell语法及KernelSU脚本执行 | 未运行，当前没有Bash/WSL运行环境 |
| Xiaomi15实机、WebView、24–48h、A/B | 未验证 |

完整 `cargo test --locked --offline --quiet` 已通过209项库单测，随后Windows以os error5拒绝启动刚生成的CLI测试程序。仅重试尚未执行的 `--bin neuroschedd --test simulated_device`，8+30项全部通过。本轮不能描述为一次完整cargo test命令无错误通过。HTTP回归在沙箱内无法绑定回环端口，经主机授权运行后通过。

基线复测为131库单测+15集成，与下方135项历史记录不同。测试命令、构建参数、SHA256、实际覆盖与限制见 [本轮交付](IMPLEMENTATION_2026-11-07.md)。

---

## 2026-10-30 历史报告

生成时间：2026-10-30
环境：Windows 11 / Git Bash / Rust 1.99.1（`stable-x86_64-pc-windows-msvc`）
交叉目标：`aarch64-unknown-linux-musl`（`rust-lld` 自包含链接，无需 NDK）

> **诚实声明**：本报告全部为**主机侧**测试结果。设备未连接（`adb devices` 为空），
> **没有任何实机测试**。所有"设备行为"结论来自合成 fixture，不是实测。

---

## 1. 结果汇总

| 项 | 命令 | 结果 |
|---|---|---|
| 格式检查 | `cargo fmt --all -- --check` | ✅ 通过（0 处需改动） |
| 静态检查 | `cargo clippy --all-targets -- -D warnings` | ✅ 通过（0 warning） |
| 单元测试 | `cargo test --lib` | ✅ **124 passed / 0 failed** |
| 集成测试 | `cargo test --test simulated_device` | ✅ **11 passed / 0 failed** |
| 合计 | `cargo test` | ✅ **135 passed / 0 failed / 0 ignored** |
| Shell 语法 | `bash -n` × 6 个脚本 | ✅ 全部通过 |
| ARM64 构建 | `cargo build --release --target aarch64-unknown-linux-musl` | ✅ 783 KiB 静态 ELF |
| ELF 校验 | 读取 e_ident / e_machine / PT_INTERP | ✅ `ELF64` / `183 (aarch64)` / 无 interp / 无 `libc.so` |
| ZIP 校验 | `scripts/build.py` 内置校验 | ✅ 11 条目、权限位、`observe_only` 默认值 |

一次 Windows 特有的执行抖动：`cargo test` 紧跟 `cargo clippy` 后首次执行偶发 `os error 5 (拒绝访问)`
（测试二进制刚被重写，被杀软/文件锁短暂占用）。`scripts/build.py` 内置一次重试，
重试后稳定通过；这不是代码缺陷，重跑 `cargo test` 亦可复现通过。

---

## 2. 单元测试分布（124）

| 模块 | 数量 | 覆盖要点 |
|---|---|---|
| `actuator::tests` | 10 | observe 不写、dry_run 只报告、enabled 写+校验、写失败回滚、静默忽略检测、外部抢写释放、越界钳制、百分比节点换算、forbidden 拦截、restore_all |
| `policy::engine::tests` | 8 | 启动宽限、前台优先、热保护抢占、内存压力不清零 min、省电阈值、息屏 IDLE、SAFE_MODE 压制、驻留防抖 |
| `collector::appevent::tests` | 6 | `wm_on_resume_called` / `wm_on_top_resumed_gained_called` / `am_proc_start` / `screen_toggled` 解析、噪声忽略、companion 两种格式、背光读屏 |
| `actuator::uclamp::tests` | 6 | v2 发现、`max` 文本值、百分比尺度、perf 尺度、异常尺度拒绝、缺失返回 None |
| `util` / `storage` / `safety::watchdog` / `predictor` / `confidence` / `memory_guard` / `json` / `config` | 各 5 | EWMA、百分位、熵；令牌生成、日志环形、CSV；崩溃循环→SAFE_MODE、健康清除、陈旧检测；马尔可夫学习与拒绝、低信任拒绝、命中率；置信度边界；换页 thrashing 判定；JSON 往返/转义/拒绝垃圾；配置钳制/损坏隔离 |
| `safety::ownership` / `thermal_guard` / `collector::thermal` / `memory` / `actuator::rollback` / `cpufreq` | 各 4 | 原始值只记一次、外部写检测、释放；滞回与封顶；充电器高温 zone 不影响 CPU 温度、毫度/度、无传感器；meminfo/vmstat 解析、zram 比例、换页速率；回滚幂等、冲突不动、写失败上报；禁写清单、opt-in 例外 |
| `types` / `safety::transaction` / `predictor::temporal` / `markov` / `collector::process` / `pressure` / `api` | 各 3 | 优先级顺序、zone 分类、模式解析；会话往返、禁用模式、轮转；周桶、平滑、往返；转移计数、自转移忽略；kswapd 差分、含空格 comm、MMD 探测；PSI 解析、缺失不伪造、畸形行；URL 解码、token 比较 |
| 其他 | 3 | `sys` 路径重定基、只读根拒写、CPU 列表解析 |

## 3. 集成测试（11）—— 端到端

fixture：`tests/common/mod.rs` 构造一棵小米 15 形态的合成树（8 核 2 簇、PSI、zRAM、MMD、
7 个 thermal zone、cgroup v2 uclamp、背光、battery、kswapd0/mmd/top-app 进程）。

| 用例 | 断言 |
|---|---|
| `observe_mode_never_writes` | 3 次 tick 后 `uclamp.min/max` 保持 `0/1024`，`report.applied == 0`，事务日志为空 |
| `detects_clusters_from_capability_not_index` | 2 簇；第一簇名 `prime`、CPU `6,7`；第二簇 `gold` |
| `hot_device_enters_thermal_guard` | 60 °C → `THERMAL_GUARD`，`uclamp_max ≤ 640`，`uclamp_min == 0` |
| `dry_run_reports_but_does_not_write` | 报告含 `planned`，文件仍为 `1024` |
| `enabled_mode_writes_verifies_and_restores_on_shutdown` | 写入 `640`、有事务记录、`shutdown()` 后恢复 `1024` |
| `external_writer_wins_and_node_is_released` | 外部改成 `512` → 报告 conflict → 节点 released → 后续 tick 与 shutdown 都不再覆盖 |
| `crashed_session_is_replayed_on_next_start` | 伪造未闭合 session → 启动时恢复 `1024`，`recovered` 含 "restored 1" |
| `crash_loop_forces_safe_mode_and_no_writes` | 连续失败达阈值 → `SAFE_MODE`，且热设备也不写入 |
| `memory_and_thermal_telemetry_is_sane` | 总内存 11264 MB、zRAM 1 盘、MMD present、PSI available、CPU 温度 51.0（不受 36 °C 充电器 zone 影响）、7 个采集器全 ok |
| `companion_events_drive_foreground_and_predictor` | companion 日志被 tail → `foreground=com.tencent.mm`、`source=companion`、首 tick `APP_LAUNCH`、冷却后 `FOREGROUND` |
| `config_reload_applies_new_thresholds` | 热阈值改到 65 °C 后，60 °C 不再触发热保护 |

---

## 4. 需求覆盖对照

| 需求条目 | 覆盖情况 |
|---|---|
| CPU 多簇识别 / 缺失节点 / 非法频率 / governor 不支持 / 权限不足 | ✅ 簇识别、缺失（`SysRoot` 空目录）、非法频率（`0`/缺失）、`available_governors` 为空、`Fault::Unreadable` |
| PSI 正常 / full 升高 / swap 激增 / kswapd 高 / zram 异常 / MMD 不存在 | ✅ 6 项均有断言 |
| 温度 正常/LIGHT/MODERATE/SEVERE / 传感器缺失 / 数据异常 | ✅ 分级 4 档 + `None` + 越界值丢弃 |
| 状态转换 / 滞回 / 冷却 / 置信度不足 / 多压力并发 / Safety 抢占 | ✅ 8 个 engine 用例 + 4 个 guard 用例 |
| Actuator 备份/写失败/回写校验/外部修改/回滚/异常退出 | ✅ 10 个用例 + 崩溃恢复集成用例 |

**未覆盖（需要实机）**：真实 `logcat` 事件流、真实 uclamp 可写性、真实温控联动、
真实 HyperOS 调度服务竞争、功耗与延迟收益。

---

## 5. 静态与打包校验

```
$ cargo fmt --all -- --check
（无输出）

$ cargo clippy --all-targets -- -D warnings
Finished `dev` profile ... （0 warning）

$ cargo test
test result: ok. 124 passed; 0 failed; 0 ignored
test result: ok. 11 passed; 0 failed; 0 ignored

$ bash -n module/*.sh scripts/audit.sh
（6 个脚本全部 OK）

$ python scripts/build.py --check
ELF aarch64 static, 783 KiB
verified 11 entries, observe_only default confirmed
neurosched-v0.1.0-arm64.zip  434 KiB
sha256 <见 dist/SHA256SUMS；zip 内含文件时间戳，每次构建不同>
```

ZIP 校验项：必需文件齐全、无绝对路径/`..`、`module.prop` 版本一致、
二进制带可执行位且为 aarch64 ELF、`config.default.json` 的 `mode == "observe_only"`。

---

## 6. 未做 / 不能做

| 项 | 原因 |
|---|---|
| 实机安装与启动 | 设备未连接 |
| 实机 `--self-test` | 同上 |
| 24–48 h 基线采集 | 同上（方法见 `docs/BENCHMARK.md`） |
| 冷/热启动延迟、帧时间 | 同上 |
| 功耗对比 | 同上 |
| WebUI 真机渲染截图 | 可在浏览器打开 `dist/stage/webui` 预览，但未在 KernelSU WebView 内验证 |

任何"实机通过"的表述都不应出现在本项目的任何材料里，直到 `docs/audit-*.txt` 与
`capability.json` 由真机产出。

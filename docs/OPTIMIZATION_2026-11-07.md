# OryonPilot v0.1.1-observe.1 优化交付

日期：2026-11-07。versionCode：102。状态：**只读观察测试包已构建并通过短时实机验证；日用、收益与完整模块生命周期验收仍为 PARTIAL。**

用户要求基于现有审计优化并自行安装。此轮没有安装/升级设备上的模块，没有停止旧模块，没有重启设备或写内核调度参数。临时测试仅创建自己的 `/data/local/tmp/oryonpilot-check.*`，结束后删除；未修改现有 `/data/adb/neurosched`。`mode="observe_only"`、`actuator.enabled=false`、`ACTUATOR_ENABLED=false` 保持不变，生产恢复路径也受写入硬门限制。观察模式会写自己的配置、日志、模型及事务元数据。

## 安装包与使用

交付目录：`C:\Users\Administrator\.codex\visualizations\2026\11\07\01a1b64d-049a-74a1-a677-65cdb87f9bf8\build-output`

- 文件：`neurosched-v0.1.1-observe.1-arm64.zip`，588856字节。
- ZIP SHA256：`07746e2b10c64499b5294de622865eaec3d35541f7b7c258d5daece4c69d336d`。
- 包内二进制 SHA256：`04edad555290f4c907c0a4469edf3fcb76a7d2437f2dd28a280fba9dd978f9d6`。
- AArch64 musl静态ELF，约966KiB，PT_LOAD对齐16384；本轮在4KiB页的小米15执行。16KiB页设备只验证ELF布局，未实机执行。
- 模块显示名为OryonPilot；文件名、模块ID、二进制及数据路径保留neurosched兼容性。不要同时安装一个不同ID的重复模块。

在SukiSU Ultra从本地选择该ZIP升级。安装保留已有配置/数据；已有stop标记也保留。安装完成后，在方便时用系统电源菜单正常重启，使新版本生效；本轮未代为重启。不要通过删除journal来解决启动拒绝。

版本应为 `neuroschedd 0.1.1-observe.1 (102)`。打开模块WebUI或 `http://127.0.0.1:8787/`，按页面提示在手机本地输入已有凭证。确认实际模式为observe_only、硬门关闭、采集时间和轮数持续增加。界面隐藏时暂停轮询，重新显示后继续。

先观察24–48小时，覆盖亮屏/息屏、充电/非充电和常用应用切换；记录卡顿、异常温度、掉电、采集停止或反复重启，再导出脱敏诊断。单次诊断是当前状态，不是24小时统计。分享前仍可自行检查内容；不要分享token、模型或原始应用日志。当前构建没有可宣称的启动加速或续航收益。

## 修复、位置与证据

| 等级 | 修改文件 / 入口 | 原因、影响与本轮处理 | 验证 |
|---|---|---|---|
| P0潜在执行风险，当前硬门已隔离 | `daemon/src/safety/transaction.rs::for_root/pending_recovery/archive_previous_boot`；`engine.rs::Runtime::new`；`main.rs::RestoreAll` | 旧恢复日志不能区分重启前后的内核实例，未来允许写入时可能把旧值回放到新启动的系统。记录并交叉校验boot_id；仅同boot日志可进入恢复，跨boot归档原证据且该次运行只观察。坏/缺身份和非空旧日志拒绝恢复；零字节旧日志允许无写迁移。 | 同boot、跨boot、坏标记、归档冲突、旧日志单测；Runtime及CLI集成 |
| P1 | 新 `safety/liveness.rs`；`safety/mod.rs`；`engine.rs::run` | 主循环本身卡住时无法自行报告失活。独立线程使用Atomic deadline，无需Runtime锁；超时结束本进程，退出码70，保留journal，由既有supervisor处理。默认任务预算30秒，合法采样等待另外计入。 | 锁占用、截止时间、取消等待及真实测试子进程exit70 |
| P1 | `safety/watchdog.rs::tick_with_health`；`engine.rs::tick` | 仅经过时间不足以证明采集健康；标健康还需当前CPU/内存/温度质量和样本时效。不是“持续90秒无异常”的证明。 | 质量差、过期及健康样本测试 |
| P2 | `engine.rs::run`；`tests/simulated_device.rs::stop_file_interrupts_a_long_sampling_wait` | stop文件原来要等完整采样间隔后才检查，配置可长达120秒。等待改为最长1秒切片并检查stop；减少旧200ms空转检查。 | 新回归在修复前失败，修复后通过；本轮实机正常停止约0.589秒。耗电收益未测 |
| P1 | `api/mod.rs`；新 `api/tests_transport.rs`；`vendor/tiny_http/**`；Cargo.toml/lock | 只限制handler无法限制上游慢HTTP头、慢体、阻塞写与解析线程。复用tiny_http0.12.0，增加8连接、固定5秒I/O期限、16KiB头、256KiB接收字节、单连接单请求、4个API worker；应用体64KiB、CSV读取16MiB。配置提交串行合并最新持久状态。 | 23项API测试，真实TCP覆盖慢头/体/滴流、配额回收、超限/截断、流水线、拒读响应、并发配置和鉴权；目标设备401/200验证 |
| P2 | `collector/cpu.rs`；`collector/memory.rs`；`engine.rs::DeviceInfo` | 已观察到governor=walt但路径不存在时显示false，页大小unknown。以精确governor补证据；展示复用已有sysconf/auxv页大小实现。 | 新回归；目标设备WALT=true、page_size="4 kB" |
| P2 | `collector/process.rs::detect_mmd` | 模糊命令行匹配可把参数中mmd或类似名称当进程。只按NUL分隔argv[0] basename精确匹配，区分二进制存在与进程运行。 | 参数、相似进程名、真实argv与binary-only回归；不把mimd2视为MMD |
| P2/P3 | 新 `collector/daemon.rs`；`collector/mod.rs`；`engine.rs` | 以前无法评估Daemon自身开销。每5秒采集自身/proc，提供CPU、RSS、线程、主线程上下文切换、采集轮数和耗时；初帧/缺失/回退输出未知并重建基线。 | 7项资源采集单测；目标设备多帧真实值 |
| P3 | `engine.rs::diagnostics_json`；`api/mod.rs`；`webui/app.js/index.html`；`webui.rs` | 加入鉴权脱敏诊断与资源展示，复用已有样式，隐藏页停止网络轮询。导出白名单不含token、应用历史、预测目标、模型、fingerprint、boot_id及自由文本日志。 | secret注入集成测试、API鉴权、JS语法；设备鉴权导出。WebView交互未验收 |
| P3 | `module/module.prop/customize.sh/action.sh/webroot/index.html`；`lib.rs`；Cargo版本；`scripts/build.py` | 显示名恢复OryonPilot，版本明确为观察测试版；iframe载入不再冒充API已连接。构建包保留项目及tiny_http许可与补丁来源。 | 5个脚本在目标Android sh语法通过；17个ZIP条目、版本/默认值/硬门验证 |

此外更新`daemon/tests/common/mod.rs`以提供明确fixture boot_id，并更新README、中文手册、CHANGELOG、PROGRESS、TEST_REPORT、DEVICE_CAPABILITY、SAFETY、原审计/交付入口及本文件。共32个已有文件变化，新增30个文件（其中26个为vendor源码/许可/说明），无删除。逐文件哈希见artifact根 `optimization-manifest.json`。无Git元数据，不能提供Git diff、提交号或声称已提交。未覆盖用户数据，也没有清理不理解的旧代码。

## 实际测试与构建

构建输出、TEMP/TMP均指向本轮artifact根，设置 `CARGO_INCREMENTAL=0`；使用现有锁文件与离线依赖。

```powershell
cargo fmt --manifest-path daemon/Cargo.toml --all
cargo clippy --manifest-path daemon/Cargo.toml --locked --offline --all-targets -- -D warnings
cargo test --manifest-path daemon/Cargo.toml --locked --offline --quiet
$env:RUSTFLAGS='-C linker=rust-lld -C link-arg=-z -C link-arg=max-page-size=16384 -C link-arg=-z -C link-arg=common-page-size=16384'
cargo clippy --manifest-path daemon/Cargo.toml --locked --offline --target aarch64-unknown-linux-musl --all-targets -- -D warnings
python -X utf8 -B scripts/test_build.py
node --check webui/app.js
```

最终Rust为**247库单测 + 8 CLI + 34集成 = 289项通过**，无失败/忽略。主机及ARM64 clippy通过；Unix专用测试仅跨编译，不宣称已运行。Python24项通过，JS语法通过。曾有2项Clippy告警，修复引用/条件表达后通过；停止等待新增回归先失败再修复。中间287/288项结果不代表最终版本。

`build_current.py`导入已有`scripts/build.py::main`，默认参数，仅指定本轮新DIST；实际执行：

```text
cargo build --locked --offline --release --target aarch64-unknown-linux-musl
cargo build --locked --offline
neuroschedd.exe --print-default-config
```

ARM64 release、主机构建及打包校验均通过；记录为artifact根 `build.log`、`build-output/SHA256SUMS`。没有skip-build，也未拿旧包冒充当前包。

目标设备始终指定 `adb -s 8d053a1f`，重新确认dada与Root uid0。`device_smoke.py`执行5个模块脚本的 `/system/bin/sh -n`；上传当前二进制并核对SHA256，然后 `timeout 20 ... --self-test`、`--once`。均exit0，自检0 failures；inspection未创建state目录。

随后用全新临时data-dir和独立端口38787运行observe_only，外层timeout60秒；ADB临时回环转发仅用于该服务。3次诊断响应间隔6秒：鉴权401/200正确，采集轮数2→3→4，7个采集器当次均ok，WALT=true、页大小4kB。CPU约0.396–0.990%（单核100%口径），RSS896–960KiB、16线程；这些是USB连接下短时快照，不是长期均值或省电结论。累计sample_errors=1且三帧未增长，启动期具体来源未在本次白名单证据中保留，不声称全程零采集异常。

自己的写事务JSONL均0字节；协作停止约0.589秒，active标记消失，记录到的logcat子进程退出；清理临时目录和ADB转发成功。证据：`device-self-test.json`、`device-inspection.json`、`device-diagnostics.json`、`device-smoke-evidence.json`。首次smoke因该内核缺少`/proc/<pid>/task/<pid>/children`中断，正常停止/清理成功；改用PID/PPID进程表后完整通过，首次记录保留于`device-smoke-attempt1.json`。没有因此更改生产源码。

## 剩余风险与验证边界

- 新包的安装、覆盖升级、卸载、冷启动、supervisor崩溃重启、SukiSU WebView交互尚未实机验收。旧版已由用户安装并运行，与本次隔离smoke是不同证据。
- 第一次用户安装后的第一屏延迟此前定位到/data F2FS检查约73.88秒，发生在Daemon启动前；本轮没有修改文件系统、fsck或重启行为，也不能保证消除该现象。
- 缺失/异常boot_id会拒绝Runtime启动，非空旧日志需人工判断。跨boot归档证据不参与常规轮转，异常频繁时需审查保留量；不要自动删除恢复依据。
- 主循环监控覆盖进入run后的进度，不能证明所有启动/内核I/O永不阻塞；5秒HTTP期限只限制socket I/O，不会取消应用锁或文件操作。单连接单请求有重连开销；大CSV导出可能碰到时间或大小上限。vendor补丁需要持续维护，上游来源和改动见 `daemon/vendor/tiny_http/PATCHES.md`。
- 未验证uclamp写入、厂商参数所有权、真实调度回滚、预测收益、自适应收敛、多用户/分屏、24–48小时稳定性、A/B温度功耗。只读界面看到uclamp可读不代表可安全控制。

## 回滚与下一阶段

模块异常时在SukiSU停用OryonPilot，保留数据和诊断。需要立刻停止采集时，可在已授权Root终端执行 `/data/adb/modules/neurosched/bin/neuroschedd --data-dir /data/adb/neurosched --stop`；只操作本模块停止标记，既有supervisor会尊重它，重启后仍保留。再次启动前须由用户明确处理stop标记，升级不会擅自清除它。

本地修改前67个文件原件：artifact根 `OryonPilot-before-optimization.zip`，SHA256 `c4c4eebaa60331038cea6a30ddbd10ad2c046cf334d1b6d92fc0d7b8819f0d94`，逐文件基线见`before-manifest.json`。按manifest选择性恢复并检查后续用户改动，不整体覆盖工作区。新增vendor/liveness/daemon/API测试等文件仅在确认无后续改动后随版本一起撤回。设备测试无剩余临时状态；不建议未经核对直接用旧版读取新版事务格式，优先停用保留数据。

下一阶段按依赖推进：先由用户安装新包并采集24–48小时只读基线，核实升级/停用/WebUI与资源开销；然后完善预测有效样本、命中/拒绝原因及简单基线对照。真实执行必须另行取得明确批准，并先确认厂商共存和设备恢复演练。Kotlin Companion与新模型仍不作为当前前置条件。

# OryonPilot 本轮工程交付

> 本文件保留最初接管实施的历史结果。当前v0.1.1-observe.1包、289项测试、小米15短时运行、安装及回滚说明见 [最新优化交付](OPTIMIZATION_2026-11-07.md)；不要使用本文件旧包哈希判断当前版本。

日期为2026-11-07。当前结论为PARTIAL，已完成可回滚的软件修复与主机验证，目标设备的日用验收待完成。完整A–D审计见 [CODE_AUDIT.md](CODE_AUDIT.md)。

## 结果

- 修复公开页面泄露凭证、日志失败后仍写入、查询入口修改活会话、重复实例、恢复失败关闭证据、停写后残留状态等问题。
- 修复cgroup百分比ABI、CPU列表、电池状态、前台事件、页大小、缺失遥测、双时钟、预测结算和Boost期限。
- 新增独立未来应用策略预览，复用AppLaunch配置及安全守卫，始终输出 `dry_run_only=true`、`writes_allowed=false`。预览单独发布至Runtime、status、predict API与WebUI。
- 209项库单测、8项CLI单测、30项集成测试通过，共247项Rust测试。另有24项Python构建/ELF测试通过。
- fmt、主机与ARM64 all-targets clippy通过。ARM64 musl release重建成功，ELF64/AArch64、静态、PT_LOAD 16KiB对齐。
- 当前ADB连接身份为ZTE MU3356、UMS9632、Android15/SDK35、6.6.98 Android15 4K内核、shell UID2000。Xiaomi15目标设备证据尚缺。

默认配置为 `mode="observe_only"`、`actuator.enabled=false`。生产 `ACTUATOR_ENABLED=false` 在 `SysRoot::write` 底层覆盖执行与恢复路径。只读预览与规则Decision采用不同类型和输出。

## 修改文件与原因

下表列出本轮已知修改。早期快照已包含部分修改，ZIP逐文件差异须结合本表理解。未发现Git元数据，无法报告分支或提交归属。

| 文件 | 原因与结果 | 主要证据 |
|---|---|---|
| daemon/src/sys.rs | 生产硬门、fixture规范路径边界、真实根识别、有限数读取；生产写日志避免无界积累 | 生产拒写、根路径、父路径逃逸测试 |
| daemon/src/config.rs | 独立actuator.enabled默认false，旧配置缺失字段关闭 | 默认、往返、旧enabled配置回归 |
| daemon/src/main.rs | inspection与运行分离、独立恢复、默认配置打印、严格参数与互斥校验、自定义配置路径 | 8项CLI单测，真实CLI只读及崩溃恢复集成 |
| daemon/src/engine.rs | 单实例、先恢复再建会话、随机nonce、停写恢复、手动rollback关闭并持久化consent、每轮校验目标、双时钟、统计与安全状态一致、独立预览输出 | 30项运行时集成 |
| daemon/src/safety/lock.rs（新增）、mod.rs | Windows文件锁与Unix flock持有生命周期，异常退出释放 | 同进程双实例与真实子进程终止测试 |
| daemon/src/safety/transaction.rs | pending落盘错误传播、完整记录校验、session校验、尾部截断、近期有界读取、保留当前会话、settled结算、拒绝会话文件复用 | 日志损坏、路径穿越、半行与会话复用回归 |
| daemon/src/safety/ownership.rs | 首次实际写前获取原始值，保留外部release | 首次基线、等值不认领、reload保留release |
| daemon/src/actuator/uclamp.rs | 按cgroup有理百分比解析小数/max，挂载确认，精确路径白名单，语义比较 | 合法小数、未知尺度、CPU控制器与路径边界 |
| daemon/src/actuator/mod.rs | 日志不持久则拒写、即时失败恢复、恢复失败禁写、持久结算、恢复顺序、外部冲突释放、实时值展示、全局clamp计划关闭 | 写失败/忽略、checkpoint失败、顺序、release与外部修改测试 |
| daemon/src/actuator/rollback.rs | 精确白名单与单位验证、失败计数、归并pending/outcome、cancelled/settled语义、冲突后封锁旧记录 | 多步重放、历史值偶合、已结算代际、非法路径/旧值 |
| daemon/src/collector/cpu.rs | 空白/逗号/范围CPU列表、共享policy去重 | 真实ABI形态fixture与拓扑测试 |
| daemon/src/collector/battery.rs | Discharging/Not charging精确识别、供电口OR、容量与温度边界 | 放电、充电及异常值测试；此修复已在早期快照中 |
| daemon/src/collector/appevent.rs | brief格式、仅确认前台事件、源与切换时间分离、失活过期、完整行、子进程停止、Linux/Android PDEATHSIG与父PID检查 | Windows事件和静默child测试通过，Unix专项测试仅跨编译 |
| daemon/src/collector/memory.rs | 实际页大小、vmstat缺失保留基线、关键meminfo验证 | 4K/16K、缺失恢复、meminfo不完整 |
| daemon/src/collector/pressure.rs、mod.rs | PSI坏值明确不可用、采集质量与墙钟 | PSI畸形/非有限/缺失及双时钟测试 |
| daemon/src/types.rs | Snapshot分离wall_ts_ms与ts_ms | Runtime、CSV和历史一致性 |
| daemon/src/policy/engine.rs | Launch硬期限、屏幕/前台失效立即退出、热更只缩短当前期限、独立电池滞回与连续息屏延迟 | 15项策略单测 |
| daemon/src/policy/prediction.rs（新增）、mod.rs | 未来目标只读预览，拒绝薄样本、低置信、坏值、缺失/过期遥测、保护态及屏幕未知 | 13项纯函数测试及Runtime发布测试 |
| daemon/src/predictor/mod.rs、markov.rs、temporal.rs | 每次前台机会一次pending，首个不同前台结算，超时失效，重复/乱序过滤，统计版本与饱和计数 | 命中/错误/超时/未来时间/持久化/旧统计迁移回归 |
| daemon/src/storage/mod.rs | CSV使用墙钟、Windows轮转重新打开、仅清理自身归档、keep_files生效、自定义配置路径 | 历史日期、日志保留和reload集成 |
| daemon/src/api/mod.rs、json.rs、webui.rs | 页面凭证隔离、Header鉴权、Host/Origin限制、拒绝启用执行器、非有限数校验、统一统计与预览字段 | 实际TCP HTTP入口与数值测试 |
| webui/index.html、app.js、style.css | 人工凭证、CSV Header fetch、移除enabled控件、评估分母、预览、单调迁移时间明确标记 | 内嵌资源测试、node语法检查 |
| module/service.sh、boot-completed.sh、action.sh | supervisor协作停止、PID身份核实、保留stop、只读查询/能力报告、有限stdout轮转 | 静态审查；Shell与KernelSU执行待验收 |
| module/customize.sh、uninstall.sh | 安全默认提示、升级保留配置、卸载核实进程/退出/恢复结果并保留全部数据 | 静态审查；真实安装/卸载待批准验证 |
| scripts/build.py、test_build.py（新增） | 强制当前locked/offline构建与打印默认值，硬门校验，真实ELF头/段/依赖/对齐校验 | 24项Python回归与当前候选出包 |
| scripts/audit.sh | ANDROID_SERIAL选择、多个设备拒绝自动选择、mountinfo及三类cgroup原值/权限 | 静态审查；当前Bash缺失，未执行 |
| daemon/tests/common/mod.rs、simulated_device.rs | fixture改为标准百分比与mountinfo，增加生命周期、安全、配置、双时钟和真实子进程故障路径 | 30项集成通过 |
| README.md、README.zh-CN.md、CHANGELOG.md | 更新当前安全状态、鉴权、模式语义、测试及兼容标识 | 直接内容核对 |
| docs/ARCHITECTURE.md、SAFETY.md、DEVICE_CAPABILITY.md、PROGRESS.md、TEST_REPORT.md | 当前实现与旧记录分层，纠正过期完成与安全声明 | 交付、测试和设备身份输出交叉核对 |
| docs/CODE_AUDIT.md、IMPLEMENTATION_2026-11-07.md（新增） | A–D审计与本轮证据、决定、未完成项和回滚 | 代码位置、测试及产物哈希 |

## 修复证据

- HTTP入口测试确认首页正文不含秘密，空凭证/query凭证/外部Origin/非法Host被拒，Header与Bearer有效，NaN配置不保存，API无法开启执行器。
- 日志文件不可写时合成节点零写入。恢复结算失败后保留active并进入只读安全态。最新记录冲突时，旧记录即使与外部值碰巧相等也不再恢复。
- `settled_rollback_is_not_replayed_after_an_external_write` 覆盖恢复完成后外部服务写回历史目标值。`restore_conflict_remains_released_across_config_reload` 覆盖热更重新认领漏洞。
- `killed_daemon_releases_lock_and_cli_recovers_the_durable_journal` 启动真实CLI进程，在合成树写入后强制终止，再由独立CLI取得锁并恢复，验证进程入口、锁释放和磁盘journal。此测试只操作fixture。
- status/once/self-test子进程在运行实例存在时保持active及节点不变，空数据目录下inspection不会创建数据。restore拒绝活实例锁。
- 手动rollback、模式降级、热信任丢失、独立consent关闭与配置热更均有Runtime回归，旧状态在fixture中恢复。

## 实际命令与结果

运行目录为工作区，Cargo命令使用 `--manifest-path daemon/Cargo.toml`。主机环境为Rust/Cargo1.99.1、Node24.20.0、Python3.14.7。

```powershell
$env:PATH = 'C:\Users\Administrator\.cargo\bin;' + $env:PATH
$env:CARGO_TARGET_DIR = 'C:\Users\Administrator\.codex\visualizations\2026\11\07\01a1b44a-2e50-7f63-ba1f-948615235d5e\audit-target'
$env:TEMP = 'C:\Users\Administrator\.codex\visualizations\2026\11\07\01a1b44a-2e50-7f63-ba1f-948615235d5e'
$env:TMP = $env:TEMP
$env:CARGO_INCREMENTAL = '0'
cargo fmt --manifest-path daemon/Cargo.toml --all -- --check
cargo clippy --manifest-path daemon/Cargo.toml --locked --offline --all-targets -- -D warnings
cargo test --manifest-path daemon/Cargo.toml --locked --offline --quiet
cargo test --manifest-path daemon/Cargo.toml --locked --offline --bin neuroschedd --test simulated_device --quiet
node --check webui/app.js
& 'C:\Users\Administrator\AppData\Local\Programs\Python\Python314\python.exe' -B scripts/test_build.py
```

fmt/clippy通过，Python24/24、JS语法通过。首次完整Rust命令运行209库单测通过后，Windows以os error5拒绝启动刚生成的CLI测试程序。最后一条定向Cargo命令仅重试未执行的部分，8项CLI和30项集成全部通过。最终覆盖247项Rust测试，不能把首次完整命令报告为零错误退出。较早一次clippy发现新增测试字段初始化风格问题，修正后当前检查通过。

HTTP测试在沙箱中bind回环端口时PermissionDenied，经已批准的主机执行后通过。所有写入测试使用合成根，测试结果不提供设备可写性或调度收益证据。

```powershell
$env:RUSTFLAGS = '-C linker=rust-lld -C link-arg=-z -C link-arg=max-page-size=16384 -C link-arg=-z -C link-arg=common-page-size=16384'
cargo check --manifest-path daemon/Cargo.toml --locked --offline --target aarch64-unknown-linux-musl --all-targets
cargo clippy --manifest-path daemon/Cargo.toml --locked --offline --target aarch64-unknown-linux-musl --all-targets -- -D warnings
cargo build --manifest-path daemon/Cargo.toml --locked --offline --release --target aarch64-unknown-linux-musl
```

三项通过。Linux/Android专项测试经过跨目标编译与clippy，未执行Unix测试二进制。Bionic/NDK构建、Rust1.80最低版本兼容性未验证。

出包通过Python导入现有 `scripts/build.py`，将 `DIST` 指向artifact下的 `build-output` 后调用main，参数为默认构建。执行前核实stage绝对路径处于该artifact根内。脚本实际运行当前host/cross构建、`--print-default-config`、ELF校验和ZIP校验。原dist目录继续保留历史产物。

## 候选产物

- 路径为 `C:\Users\Administrator\.codex\visualizations\2026\11\07\01a1b44a-2e50-7f63-ba1f-948615235d5e\build-output\neurosched-v0.1.0-dev-arm64.zip`。
- 大小为569681字节，11个ZIP条目。ELF约964KiB，AArch64 ET_EXEC，无PT_INTERP和DT_NEEDED，PT_LOAD最小对齐16384。
- SHA256为 `5bceb25b63a1abf4dd83b4822b7be16a813cf81f5c4215c73ec0e2b8c8927306`。
- `config.default.json` 校验 `mode=observe_only`、`actuator.enabled=false`，构建源硬门校验false。该包属于待设备验收候选，版本和兼容标识保持当前工程值。

## 重要工程决定

1. 项目沿用OryonPilot名称，`neurosched`二进制、Cargo包、模块ID与数据目录作为已有兼容标识保留。内部迁移另需兼容方案。
2. WebUI凭证由持有者人工输入，只在页面内存保存，URL凭证拒绝。已有Header/Bearer客户端继续使用。
3. 默认软件consent关闭，生产硬门关闭。全局sched_util_clamp配置保留诊断用途，执行计划关闭。冲突始终主动release，旧ignore配置不会取得覆盖外部值的资格。
4. 卸载保留配置、历史、模型和journal。stop标记跨重启保留，需要持有者显式清除后恢复服务。
5. 打包强制当前源码构建，`--skip-build`明确拒绝。
6. 预测统计采用version2，accuracy为hits/evaluated。旧轮询口径计数保留在legacy_stats，新评估重新累计。模型重置动作仍由显式API操作触发。
7. 预测预览复用既有AppLaunch profile和持续时间，目标限定未来应用。真实启动延迟、功耗和调度收益未作为统计推断值填入。

## 未验证与遗留问题

| 级别/阶段 | 尚缺证据或实现 | 后续处理 |
|---|---|---|
| P1 Phase0/3 | journal缺少boot_id绑定，旧日志/跨启动恢复兼容仍有歧义；生产硬门下待恢复会话可能阻止Runtime启动 | 执行资格恢复前完善boot边界、迁移与只读诊断保留策略 |
| P1 Phase3 | Xiaomi15接口、SELinux、实际写入者、Scene/Power HAL共存、读改写竞态和同值ABA | 先只读核实，受控恢复和写入试验需明确批准 |
| P1 Phase4 | 缺少实测启动延迟、能耗与收益反馈，模型容量上限、衰减和自适应调整未完成 | 建立可核验指标后设计反馈，保留原统计模型 |
| P2 Phase0 | watchdog仍在采样线程，无法检测该线程自身卡死，健康判定仍偏时间驱动 | 独立有界活性检测与故障注入 |
| P2 API | 单线程慢请求体可能阻塞控制台，连接配额和读超时未完善 | 复用HTTP库能力后做真实慢客户端测试 |
| P2 长稳 | 活跃journal和模型长期容量、持续线程/FD/RSS、24–48h重启与存储故障 | 有界持久策略与长期主机/设备观测 |
| P2 配置 | apps.event_source、apps.companion_log、game.detect_by_gpu_load、safety.watchdog_interval_ms等仍有未消费字段 | 按真实数据来源补实现或在UI明确禁用 |
| P2 遥测 | 各collector的质量覆盖尚未统一到每个字段，部分CPU异常值仍可能表现为零 | 缺失/陈旧/异常样本统一语义与更多正常入口测试 |
| 设备生命周期 | PDEATHSIG在真实logcat凭据属性下是否保留、KernelSU升级/卸载返回码、WebView兼容 | 目标设备专项验收；当前Shell运行环境缺失 |
| Phase5 | 响应、启动、唤醒、频率、温度、能耗、长稳A/B | 审核基线与受控实验条件后进行，当前无收益数据 |

值比较与写入之间缺少内核原子所有权协议。settled修复了已复现的旧事务重放错误，厂商共存仍属待验证边界。

## 只读设备验证步骤

先确认目标设备身份。以下命令读取设备信息，输出可在本地保存。`ls`权限位不能替代SELinux和实际接口可写性证明。

```powershell
adb devices -l
$serial = '<目标Xiaomi15序列号>'
adb -s $serial shell 'getprop ro.product.manufacturer; getprop ro.product.model; getprop ro.product.device; getprop ro.soc.model; getprop ro.build.version.release; getprop ro.build.version.sdk; uname -r; id'
adb -s $serial shell 'cat /proc/self/mountinfo; cat /proc/cgroups'
adb -s $serial shell 'cat /sys/devices/system/cpu/present; cat /sys/devices/system/cpu/online; for p in /sys/devices/system/cpu/cpufreq/policy*; do echo "$p"; cat "$p/related_cpus" "$p/scaling_cur_freq" "$p/cpuinfo_max_freq"; done'
adb -s $serial shell 'for d in /sys/fs/cgroup/top-app /sys/fs/cgroup/cpu/top-app /dev/cpuctl/top-app; do ls -ln "$d/cpu.uclamp.min" "$d/cpu.uclamp.max"; cat "$d/cpu.uclamp.min" "$d/cpu.uclamp.max"; done'
adb -s $serial shell 'cat /proc/meminfo; cat /proc/pressure/cpu; cat /proc/pressure/memory; cat /proc/vmstat'
adb -s $serial shell 'for z in /sys/class/thermal/thermal_zone*; do echo "$z"; cat "$z/type" "$z/temp"; done; cat /sys/class/power_supply/battery/status /sys/class/power_supply/battery/capacity'
adb -s $serial shell 'ps -A; getprop init.svc.mmd; ls -ln /system/bin/logcat'
```

有Bash的主机可在确认serial后运行已有检测工具。

```bash
ANDROID_SERIAL='<目标Xiaomi15序列号>' bash scripts/audit.sh --stdout
```

当前主机未安装可用Bash/WSL，`audit.sh`及模块脚本本轮仅静态审查。权限不足、节点不存在、映射不明均应记录为未知。目标设备上的软件复制、执行、安装与后续写入按各自授权另行安排。

## 回滚与快照边界

早期保护快照位于 `C:\Users\Administrator\.codex\visualizations\2026\11\07\01a1b44a-2e50-7f63-ba1f-948615235d5e\OryonPilot-pre-fix-20261107.zip`，SHA256为 `306218fe0416114b1819bf5e8fd054a6461d2f6ff006a0733897a686567de0e3`。

该ZIP在并行修复开始后形成，已包含电池采集和部分底层硬门修复。它能提供初期文件保护，完整恢复接管前状态无法保证。恢复到该快照也会恢复当时仍存在的API/事务缺陷，应作为代码比对和局部回退来源。

| ZIP内路径 | 工作区恢复映射 |
|---|---|
| src/* | daemon/src/* |
| tests/* | daemon/tests/* |
| Cargo.toml、Cargo.lock | daemon/Cargo.toml、daemon/Cargo.lock |
| module/*、scripts/*、webui/*、docs/* | 同名工作区目录 |
| README.md、README.zh-CN.md、CHANGELOG.md、.gitignore | 工作区同名文件 |

将ZIP解到独立比对目录，按 `repair-manifest.json` 的路径和当前SHA256核对，仅回退选定文件。后续用户修改应先合并。新增文件为 `safety/lock.rs`、`policy/prediction.rs`、`scripts/test_build.py`及两份本轮报告，回退时同步移除其引用。保留源码快照、manifest及测试报告供追溯。

本轮首次测试误将TEMP指向daemon目录，生成66个fixture目录。进入实施后已核对绝对路径并清理本轮生成目录，后续临时数据均位于artifact根。

## 下一阶段

先补boot边界、独立活性检测、慢请求限制、容量约束与未生效配置，再取得目标设备只读数据。随后进行24–48h只读基线与预测Dry Run，核对事件质量和统计口径。Phase3执行资格取决于接口/共存/恢复审查与用户明确批准，Phase4–5依赖真实收益指标和长稳证据。

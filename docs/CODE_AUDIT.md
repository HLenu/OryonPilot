# OryonPilot 接管审计

审计日期为 2026-11-07。结论为 PARTIAL，工程尚未达到日用验收条件。

本文A–D记录最初接管时的基线。此后的目标小米15取证见DEVICE_CAPABILITY；v0.1.1-observe.1新增问题、修复、最终289项测试、观察包与剩余风险见 [优化交付](OPTIMIZATION_2026-11-07.md)。下方旧设备状态与行号不可代替当前证据。

## A. 项目现状与证据边界

- 工作区没有 `.git`。分支、提交与未提交修改归属无法核实。实施初期保护快照名为 `OryonPilot-pre-fix-20261107.zip`，SHA256 为 `306218FE0416114B1819BF5E8FD054A6461D2F6FF006A0733897A686567DE0E3`，位于本次 Codex visualizations 工作目录。核对ZIP确认其中已包含电池采集和部分底层硬门修复，无法据此保证完整恢复接管前状态。文件名中的pre-fix不能作为时间边界证据。
- 源码、模块 ID、数据目录和 Cargo 包名使用 NeuroSched/neurosched。旧 target/dist 同时存在 OryonPilot 产物。历史品牌与当前源码不一致，不能据旧二进制推断当前实现。内部路径迁移需要兼容方案。
- Rust 单进程 Runtime 串联 collectors、Markov/temporal/EWMA predictor、policy、actuator、journal。HTTP/WebUI 使用 tiny_http，JSON 解析使用 serde_json，随机令牌使用 getrandom，Unix 信号使用 signal-hook。Cargo.toml 的“零依赖”历史文档已失效。
- KernelSU 包含安装、late-start supervisor、boot-completed、action、卸载和 iframe WebUI。companion 目录为空，没有 Kotlin 工程、Gradle 或可编译 APK。
- 审查范围覆盖全部 Rust 源文件与测试、两份 README、CHANGELOG、docs 下九份原有文档、Cargo manifest/lock、构建和设备审计脚本、模块脚本与 WebUI。没有项目级 AGENTS.md。
- 修改前实跑 `cargo test --locked --offline --quiet`，131 单测、15 集成测试通过。`cargo fmt --all -- --check`、`cargo clippy --locked --offline --all-targets -- -D warnings`、ARM64 musl release 构建通过。构建在独立 artifact target 下进行。
- 初次沙箱内 `adb devices -l` 无法启动 server。获准在主机列设备后，只读身份检查确认连接的是 ZTE MU3356、UMS9632、Android 15 / SDK35、6.6.98 Android15 4K 内核，shell UID2000。目标 Xiaomi 15 尚不可用，节点权限、mountinfo、厂商写入、性能与功耗仍未经目标设备验证。
- 初次测试把 TEMP 设到了 daemon 目录，生成了 66 个测试 fixture 目录。进入实施后已核对范围并清理。之后临时数据统一放 artifact 目录。

## B. 问题清单

下列行号指审计时的源码基线。实施会移动行号，函数名用于继续定位。修复状态见 PROGRESS.md 与本轮实施记录。P0 表示必须在任何设备执行资格恢复前解决。

| ID / 级别 | 文件与基线位置 | 原因和复现证据 | 影响 | 推荐修复 |
|---|---|---|---|---|
| S01 P0 | daemon/src/api/mod.rs 63、93，handle | 未鉴权首页将 shared.token 注入 HTML，鉴权在静态页返回之后 | 本机普通网络客户端获得 root 控制 API 凭证及应用使用历史访问权 | 停止公开凭证，采用独立持有凭证的 Header 认证，补 HTTP 入口测试 |
| S02 P0 | daemon/src/actuator/mod.rs 393，apply | `let _ = journal.append(pending)` 后仍写节点，磁盘满、权限失败可复现 | 调度变化没有持久恢复证据 | fsync/append 失败立即拒绝写入，测试不可写日志 |
| S03 P0 | daemon/src/main.rs 165、179、190，engine.rs 297 | status/restore 创建 Runtime、覆盖 active 并采样，未取得进程锁 | 查询可恢复运行中会话、覆盖 journal/PID、执行策略。两个实例可竞争同一节点 | 分离纯只读 CLI 与独立恢复路径，使用内核进程锁 |
| S04 P0 | engine.rs 297、299、747，module/uninstall.sh 49、53 | 先覆盖 active 再恢复，失败也关闭 journal。卸载吞掉恢复退出码并删 DATA | 原始状态和恢复证据丢失，异常状态可能无法自动恢复 | 先恢复成功再开启新会话，失败保留 active 和数据，卸载检查结果 |
| S05 P1 | actuator/uclamp.rs 78–112，discover | 当前值 >100 猜 1024 尺度，`max` 当 1024，小数不能 parse u32。合法 `0.00/max` 可复现 | 错单位写入，或正常接口完全不可用。整数 fixture 隐藏 ABI 问题 | 按确认的 cgroup ABI 解析 rational percent，验证挂载，未知接口拒写，语义化回读比较 |
| S06 P1 | engine.rs 507–526、629–641 | 退出 enabled 仅 set_mode，rollback 后同 tick 可 apply | UI 显示停写但旧 boost/cap 残留，回滚按钮失效 | 退出写模式先恢复，检查失败，手动回滚同时关闭执行资格 |
| S07 P1 | engine.rs 516、665，api/mod.rs 435 | 仅 state_changed 或60秒 apply，执行器开关热更新没有送到 Actuator | 同态温度 ceiling、profile、更改开关滞后或失效 | 检查完整目标与所有权，更新执行器配置，关闭时恢复 |
| S08 P1 | config.rs 257，types.rs RunMode | 只有 mode 控制写入，默认 top-app 功能开，无独立 ACTUATOR_ENABLED | 旧 enabled 配置或 API 可直接取得设备写资格 | 默认 actuator.enabled=false，生产硬闸门false，所有写路径共用底层拒写 |
| S09 P1 | json.rs 64，config.rs 812，api/mod.rs 374 | 字符串 NaN 经 as_f64 后参与 clamp，可造成 panic/abort | 本地配置或 API 导致 daemon 崩溃循环 | 数值边界拒绝非有限值，API 原子校验后保存 |
| S10 P0 | actuator/rollback.rs replay，actuator/mod.rs restore_all，engine.rs restore_owned | 恢复后旧日志继续active；最新记录冲突后仍重放同节点旧记录；恢复冲突后forget可重新写入。新增进程/合成回归复现历史值再次匹配场景 | 重放已结算事务或覆盖外部服务状态 | 持久settled、失败事务cancelled、归并pending与完成记录，冲突封锁旧记录并保留release，结算失败禁写且保留active |
| S11 P1 | engine.rs Runtime::new，transaction.rs begin | 原会话ID仅mono时间与PID，跨启动可能重用，且相同session会跳过恢复 | 跳过未完成恢复或复用历史文件 | 加随机nonce、无条件审查pending，create_new拒绝会话复用 |
| S12 P1 | safety/transaction.rs JournalEntry，engine.rs Runtime::new | journal未绑定设备boot_id，跨设备重启后路径和值偶合时无法判断原会话是否仍适用，属代码证据与条件推演，未做断电实验 | 未来开放真实恢复时可能使用旧启动基线 | 开放执行前增加boot边界与旧日志迁移规则，当前生产硬门持续关闭 |
| T01 P1 | sys.rs 218，collector/cpu.rs 192–197 | CPU list 仅按逗号分隔，`0 1 2 3 4 5` 返回空 | 实际 cpufreq policy 被跳过，拓扑/频率页失真 | 支持空白与逗号、范围，真实 ABI fixture，per-CPU fallback 按 CPU 集合去重 |
| T02 P1 | collector/battery.rs 56 | contains("charg") 匹配 Discharging 和 Not charging | 放电设备被判充电，低电量策略失效 | 标准状态精确匹配与容量范围验证 |
| T03 P1 | collector/appevent.rs 125、308 | logcat启动 raw，解析却要求 tag | 正常事件入口无法解析，单测绕过了命令格式 | 使用保留 tag 的格式，测试匹配实际输出 |
| T04 P1 | collector/appevent.rs 109、280 | am_proc_start 全部作为前台 Launch | 后台 service/provider 改写前台、触发 Boost、污染训练 | 仅确认的 resumed/foreground 事件更改前台 |
| T05 P2 | collector/appevent.rs 155、226–248 | 用前台切换时间判断源活性，启动即称 logcat，fallback 继承高信任，失活无 TTL | 全 /proc 周期扫描、应用时长失真、旧前台永久参与策略 | 分离观测与切换时间、逐事件来源、失活降级，热更实际线程间隔 |
| T06 P2 | collector/appevent.rs 195，stop 78 | 半行即消费offset，stop只改标记，阻塞读未被中断 | Companion事件丢失，正常停止后子进程残留风险 | 仅提交完整行，持有Child并显式kill/wait；异常父退出单独验证 |
| T07 P2 | collector/memory.rs 63、72、86 | 固定4 KiB页，vmstat失败以0覆盖基线 | 16 KiB swap速率低估4倍，恢复时产生假峰值 | 查询真实页大小、只更新成功基线、注入4K/16K测试 |
| T08 P2 | collector/cpu.rs 66，memory.rs 46，pressure.rs 15，collector/mod.rs 146 | 负载/PSI坏值归零，meminfo任意字段即ok | 缺失数据伪装空闲/零压力或误报低内存 | 验证关键字段、有限范围，明确采集质量和时间 |
| P01 P1 | engine.rs 472、701，collector/mod.rs 123，appevent.rs 169 | tick是进程单调时间，事件是epoch，混用于预测 | 时间桶错误、真实命中无法统计、recency错误 | 显式wall_ts_ms与monotonic分离，双时钟正常路径测试 |
| P02 P1 | predictor/mod.rs 152、283 | 5秒刷新增加分母并覆盖pending，错误下一应用不结算 | 准确率随轮询变化，迟到目标误记命中 | 每次前台机会去重，首个不同前台结算，超时失效，保留旧统计来源 |
| P03 P1 | engine.rs 482，policy/engine.rs 88 | evaluate不接收Prediction，预测仅放UI | 预测没有生成策略和收益反馈 | 先独立只读预测策略模拟及评估，再审查是否允许进入执行 |
| P04 P2 | policy/engine.rs 129、144、175，types.rs min_dwell | exit_pct、息屏delay未消费，2.5秒Boost受4/8秒驻留冷却延长 | 持续Boost与配置语义不符 | 电池滞回、息屏延迟、Boost硬期限独立于普通防抖 |
| L01 P2 | storage/mod.rs 311、354，webui/app.js new Date | Snapshot单调时间当epoch | CSV日期1970，历史图和迁移时间错误 | 持久数据使用墙钟，单调时间另标记 |
| L02 P2 | storage/mod.rs 241，config.rs log.keep_files | 日志只轮转不清理，当前journal每tick全读，模型表无上限 | 长期磁盘、内存、采样耗时增长 | 按归档保留上限清理，journal有界读取，模型容量/衰减设计 |
| L03 P2 | safety/watchdog.rs tick，engine.rs watchdog.tick | 用刚生成snap.ts检查stale，卡住的同线程不能tick | 无法检测真实采样死锁，时间到即健康不反映采集质量 | 独立活性检测和质量健康标准，实机故障注入验证 |
| U01 P2 | actuator/mod.rs capability_json，webui/app.js 565 | 当前uclamp展示使用启动缓存 | UI与实际自身/外部修改不一致 | 区分基线和实时只读值、加入观测时间和可读性 |
| U02 P2 | api/mod.rs read_body/serve | 单线程无限等待慢请求体 | 一个慢客户端阻塞控制台 | 使用可验证超时/连接配额，避免引入无界线程 |
| Q01 P3 | README、docs/TEST_REPORT、PROGRESS、Cargo.toml | 135历史测试、零依赖、完成声明与当前146测试及依赖不一致；缺失被引用文档 | 无法追溯构建、误判完成度 | 更新当前证据，历史报告保留日期，验证构建来源 |

未使用/无效接口还包括 apps.event_source、apps.companion_log、game.detect_by_gpu_load、safety.watchdog_interval_ms 等。未发现 Kotlin 双重写入实现。Rust 写调度的主要入口集中 Actuator，但旧 recovery 绕过 denylist。外部 Scene/Power HAL/WALT 是否写同一节点只能列为未验证，不能仅凭进程名证明冲突。

## C. 完成度矩阵

| 阶段/能力 | 审计时状态 | 验证范围 |
|---|---|---|
| Phase0安全、事务、生命周期 | 设计存在问题 | 有合成测试，但未覆盖上列P0 |
| Phase1 CPU/内存/热/电池 | 部分实现 | 主机夹具通过，实际ABI和质量边界有缺陷 |
| Phase1前台识别 | 部分实现 | Companion合成入口可工作，logcat正常入口错误 |
| Phase1 uclamp/调度状态 | 已实现但未经设备验证 | 只发现路径，不证明ABI/可写/厂商共存 |
| Phase2规则状态机 | 已完成且主机验证 | 基本优先级和守卫，有期限/热更缺陷 |
| Phase2统计预测 | 部分实现 | 模型单测通过，运行时时钟与评估错误 |
| Phase2预测策略模拟与收益评价 | 尚未实现 | 没有正常输入至策略的闭环 |
| Phase3受控执行 | 设计存在问题 | 数值、权限、恢复边界未具备执行资格 |
| Phase4自适应与收益反馈 | 尚未实现 | 无设备收益指标、模型衰减/学习策略 |
| Phase5 A/B与发布 | 尚未实现 | 仅历史基准方案，无实测数据 |
| Companion | 尚未实现 | 可选输入文件通道存在，没有APK |

## D. 依赖顺序与验收

| 顺序 | 修改文件 | 预期结果与测试 | 风险 | 实机要求 |
|---|---|---|---|---|
| 1 | sys.rs、config.rs、api/*、webui/* | 生产硬闸门false、旧配置安全、凭证隔离、HTTP401/403与NaN回归 | 低，可本地回滚 | WebView交互待测 |
| 2 | main.rs、safety/*、actuator/*、engine.rs、module/*.sh | 查询零副作用、单实例、日志失败拒写、失败保留恢复证据、停写即恢复；正常CLI/故障注入测试 | 中，影响生命周期 | 未来需独立批准的受控恢复演练 |
| 3 | uclamp.rs、tests/common、simulated_device | 百分比/小数/max与真实挂载匹配、未知ABI拒写、同态变化/热失信回退 | 中 | mountinfo、权限与回读语义必须确认 |
| 4 | collector/*、sys.rs | 修复CPU列表、电池、事件、缺失恢复、页大小；4K/16K、半行、源失活回归 | 低 | 真实logcat、热zone、频率和前台核对 |
| 5 | engine.rs、types.rs、predictor/*、storage/* | 双时钟、机会结算、有界日志；epoch事件与mono tick集成测试 | 低 | 24–48h只读运行 |
| 6 | policy/*、engine.rs、API/UI | 短时Boost硬期限、电池滞回、独立预测Dry Run与质量标识 | 中 | 只读场景核验 |
| 7 | watchdog、连接管理、journal、模型 | 活性检测、慢请求约束、长期有界、故障与持续运行测试 | 中 | 卡死/断电场景需后续授权方案 |
| 8 | 只读audit工具、DEVICE_CAPABILITY、BENCHMARK | 取得真实身份、挂载、节点读值、竞争服务与功耗基线 | 只读低风险 | 需要设备和ADB可用 |
| 9 | Phase3–5 | 实际写入、A/B、发布 | 高 | 必须用户明确批准，且满足前置验证 |

## 官方依据

cgroup v2 的 cpu.uclamp.min/max 使用有理百分比，max 表示无封顶。CPUFreq policy 提供相关CPU列表和kHz频率。此处确认ABI文档，设备是否实现相同ABI仍需挂载和实际节点证据。[Linux cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html#cpu)，[CPUFreq](https://docs.kernel.org/admin-guide/pm/cpufreq.html)

uclamp是性能请求约束，runqueue聚合会影响整机效果，不能由写成功推断功耗或延迟收益。[Linux Utilization Clamping](https://docs.kernel.org/scheduler/sched-util-clamp.html)

KernelSU规定service、boot-completed和卸载脚本的生命周期，现有模块可复用。[KernelSU Module Guide](https://kernelsu.org/guide/module.html)

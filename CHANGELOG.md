# Changelog

All notable changes to this project are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning follows SemVer.

## [0.1.1-observe.1] - 2026-11-07

- Bind recovery journals to kernel boot_id. Archive previous-boot evidence without replay; reject ambiguous nonempty legacy journals.
- Add independent main-loop liveness monitoring, health-aware boot marking and prompt stop-file handling during long sample waits.
- Bound HTTP connections, worker count, I/O lifetime, headers, request bodies and CSV exports using a documented tiny_http 0.12.0 patch; serialize configuration commits.
- Correct WALT/page-size capability display and MMD process matching; add real daemon resource counters and authenticated, redacted diagnostic export.
- Pause hidden WebUI polling and restore the OryonPilot display name while preserving internal compatibility identifiers.
- Keep production kernel writes disabled. 289 Rust and 24 packaging tests passed; ARM64 read-only device smoke passed. Long-term and performance acceptance pending.
- Include project license and the vendored HTTP dependency's licenses/provenance in the ZIP. See `docs/OPTIMIZATION_2026-11-07.md` for evidence and rollback.

## [0.1.0] - 2026-10-30

First release. Scope is deliberately narrow: observe everything, decide in one place,
write almost nothing.

### Added

**Daemon (`neuroschedd`)**
- Zero-dependency Rust daemon, single static aarch64 ELF (783 KiB), built with
  `aarch64-unknown-linux-musl` + `rust-lld` (no Android NDK required).
- Path-injected filesystem layer (`SysRoot`) with fault injection
  (`Unreadable` / `Empty` / `WriteFail` / `WriteSilentlyIgnored`) so the whole daemon
  runs against a synthetic device tree in CI.
- Collectors: CPU (per-core load, cpufreq policies, capability-ordered cluster naming),
  memory (`meminfo`/`vmstat`/`swaps`/zRAM `mm_stat`, swap-in KiB/s), PSI (`some`/`full`),
  thermal (zone classification by `type`, never by index), battery, process
  (`kswapd0` CPU share, MMD detection), app events.
- App event pipeline with three tiers: `logcat -b events` (long-lived child, no polling)
  → companion `fg_events.log` (incremental tail) → `/proc` `top-app` scan
  (low trust, caps prediction confidence at 50%).
- Predictor: first-order Markov + hour-of-week profile + launch-interval EWMA, fused and
  scored; refuses to answer below sample/confidence thresholds; tracks its own hit rate.
- Policy: explicit state machine with 9 states, priority resolution, enter/exit
  hysteresis, minimum dwell, cooldown, protective-state preemption, and a reason string
  on every transition.
- Thermal and memory guards that can only *lower* the ceiling; the predictor can never
  override them.
- Actuator: eight-step transaction (read → ownership → validate → journal+fsync → write
  → read back → verify → restore on failure) restricted to `top-app`
  `cpu.uclamp.min/max`, plus an opt-in global clamp that is off by default.
- uclamp **scale detection**: cgroup nodes are `0..100` percent, sysctl is `0..1024`;
  the scale is measured from the node's current value and profile values are converted
  (`128 → 13`, `640 → 63`) instead of being written raw.
- Hard denylist (`forbidden()`) covering CPU frequency/online/affinity, thermal,
  zRAM, `vm.*`, LMKD, MMD, schedtune, debugfs, GPU, `/sys/power`.
- Safety: node ownership, conflict detection with automatic release, JSONL write
  journal with crash replay, boot watchdog with crash-loop detection and `SAFE_MODE`,
  config corruption quarantine, cooperative stop file, uninstall restore.
- Local HTTP API on `127.0.0.1` with token auth (constant-time compare), 12 endpoints,
  and no endpoint that can execute a shell command.

**Module**
- KernelSU module with `module.prop`, `customize.sh`, `service.sh`,
  `boot-completed.sh`, `uninstall.sh`, `action.sh`, `webroot/`.
- Installer verifies the binary runs (`--version`) and writes a default config only when
  none exists; upgrades never overwrite tuned thresholds.
- `boot-completed.sh` writes a read-only capability report to `logs/capability.json`.

**WebUI**
- Dark, responsive console with 7 tabs (Dashboard / CPU / Memory / Predictor / Policy /
  Safety / Logs), three files, no framework, no CDN, no external fonts.
- Canvas history chart, per-cluster and per-core bars, PSI bars, node ownership table,
  transaction journal, threshold editor, mode switch, rollback button.

**Tooling**
- `scripts/build.py`: fmt + clippy + test gates, cross-compile, ELF verification,
  config generation from the daemon itself, staging, zip with unix permissions,
  archive verification (no absolute paths, executable bit, aarch64 ELF,
  `observe_only` default), SHA256SUMS.
- `scripts/audit.sh`: strictly read-only ADB device audit.

**Docs**
- `docs/OPEN_SOURCE_RESEARCH.md` (licenses, versions, reuse decisions),
  `docs/DEVICE_CAPABILITY.md` (unverified, marked as such),
  `docs/ARCHITECTURE.md`, `docs/SAFETY.md`, `docs/TEST_REPORT.md`,
  `docs/BENCHMARK.md` (methodology only, no fabricated numbers), `docs/PROGRESS.md`,
  `README.md`, `README.zh-CN.md`.

### Known limitations
- No real-device verification: no phone was connected during development.
- `uclamp.max` cannot cap the whole runqueue because of the kernel's max-aggregation
  semantics; only `top-app`'s own requests are affected.
- Thermal protection is inactive when no thermal zone matches a CPU-class keyword.
- No prebuilt Kotlin companion APK (no Android SDK in the build environment); its file
  channel (`fg_events.log`) is implemented and tested.

### Security
- Default mode is `observe_only`; the packaged config is verified to be `observe_only`
  at build time.
- No `chmod 777`, no SELinux changes, no partition writes, no reboot calls, no network
  client code.
# 2026-11-07 接管修复

完成当前代码审计和首轮安全、遥测、策略及预测反馈修复，新增独立只读预测策略预览。247项Rust和24项构建脚本主机测试通过，ARM64 musl重建完成。目标设备与日用验收待完成。默认observe_only、独立consent关闭、生产ACTUATOR_ENABLED=false。详细变更与快照局限见 [本轮交付](docs/IMPLEMENTATION_2026-11-07.md)。


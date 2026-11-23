# OryonPilot

2026-11-07 v0.1.1-observe.1 只读观察测试版。现有 `neurosched` 包名、模块 ID、二进制名和数据目录继续作为兼容标识。289 项 Rust 回归通过，新版已在目标小米15执行自检、短时隔离采集与鉴权 API 测试，长期日用验收待完成。改动、安装、证据和回滚见 [本轮交付](docs/OPTIMIZATION_2026-11-07.md)；初始审计见 [审计报告](docs/CODE_AUDIT.md)。

安全默认值为 `mode="observe_only"`、`actuator.enabled=false`、`ACTUATOR_ENABLED=false`。当前构建的生产根写入硬门同时覆盖执行和恢复。预测策略通过独立只读预览输出。

> 骁龙 8 Elite / 小米 15 的**可解释、可回滚**资源调度系统。
> Rust 单进程守护 + KernelSU 模块 + 本地 WebUI。**默认只读，安装后不修改任何内核参数。**

[![license](https://img.shields.io/badge/license-GPL--3.0--or--later-blue)](LICENSE)
[![tests](https://img.shields.io/badge/tests-289%20passed-brightgreen)](docs/TEST_REPORT.md)
[![device](https://img.shields.io/badge/device-readonly%20smoke%20passed-yellow)](docs/DEVICE_CAPABILITY.md)

中文使用手册：[README.zh-CN.md](README.zh-CN.md)

---

## What it is

A telemetry daemon that watches CPU clusters, PSI, zRAM, thermal zones and foreground
apps, runs an explicit state machine, and publishes policy and future-app previews.
The current production build enforces read-only kernel access.

It is deliberately *not* a tuning script:

- no CPU frequency / governor / affinity writes
- no thermal threshold tampering
- no zRAM / LMKD / swappiness writes (MMD owns those on Android 17)
- no shell execution from the UI
- no network access; the API binds `127.0.0.1` only

## Status (honest)

| Area | State |
|---|---|
| Safety and telemetry repairs | host regression verified |
| Host test suite | 289 Rust tests and 24 build-script tests passed |
| ARM64 musl | rebuilt, static ELF, 16 KiB PT_LOAD alignment |
| Target device verification | Xiaomi15 / Android17 / SukiSU Ultra: read-only smoke passed; long-run acceptance pending |

The target's CPU topology, readable cgroup ABI, WALT, page size and telemetry were
checked on-device. Uclamp writes, vendor coexistence and performance gains remain
unverified. See
[docs/DEVICE_CAPABILITY.md](docs/DEVICE_CAPABILITY.md).

## Device validation

Use the observation package and installation checks in the current delivery report.
The user installs and reboots manually; kernel-write experiments require separate approval.
The existing dist directory contains historical artifacts. The current candidate is
stored separately and identified by SHA256 in the delivery report.

## Architecture at a glance

```
collectors → snapshot → predictor → policy(state machine) → actuator → journal
                │                        │                      │
            procfs/sysfs          thermal+memory guards    ownership + verify + rollback
```

Read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full design,
[docs/SAFETY.md](docs/SAFETY.md) for the guarantees and the rollback ladder.

## Build

```bash
rustup target add aarch64-unknown-linux-musl
python scripts/build.py --check      # fmt + clippy + test + cross-compile + zip + verify
```

Produces `dist/neurosched-<ver>-arm64.zip` and `dist/SHA256SUMS`.
No Android NDK needed: the binary is a static musl ELF linked with `rust-lld`.

## Repository layout

```
daemon/      Rust crate (locked serde_json, tiny_http, getrandom, libc, signal-hook)
  src/collector/  cpu, memory, pressure, thermal, battery, process, appevent, daemon
  src/predictor/  markov, temporal, confidence
  src/policy/     engine, state machine, thermal/memory guards
  src/actuator/   uclamp, forbidden-node list, rollback
  src/safety/     ownership, boot-bound journal, watchdog, liveness
  vendor/tiny_http/  existing parser with bounded transport patch and upstream licenses
  src/api/        loopback HTTP API
  tests/          synthetic Xiaomi 15 tree + end-to-end tests
module/      KernelSU module (scripts + webroot)
webui/       embedded console (3 files, no framework, no CDN)
scripts/     build.py, audit.sh
docs/        research, capability, architecture, safety, tests, benchmarks, progress
dist/        build artifacts
```

## License

GPL-3.0-or-later. The existing tiny_http 0.12.0 dependency is now vendored with a
narrow transport-limits patch, retaining its MIT/Apache-2.0 notices in source and
the module ZIP. See [patch provenance](daemon/vendor/tiny_http/PATCHES.md) and
[earlier research](docs/OPEN_SOURCE_RESEARCH.md). Other dependencies remain locked.

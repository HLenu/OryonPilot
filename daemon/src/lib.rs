//! NeuroSched - safety-first, telemetry driven resource policy for Snapdragon 8 Elite.
//!
//! Design rules enforced by this crate:
//!  * Nothing is written unless the actuator is explicitly enabled *and* the node
//!    ownership check passes.
//!  * Every sysfs/procfs access goes through [`sys::SysRoot`] so the whole daemon
//!    can be exercised against a synthetic device tree in tests.
//!  * The release binary must be a single static ELF that can
//!    be dropped into a KernelSU module without shipping a runtime.

pub mod actuator;
pub mod api;
pub mod collector;
pub mod config;
pub mod engine;
pub mod json;
pub mod learner;
pub mod policy;
pub mod predictor;
pub mod safety;
pub mod storage;
pub mod sys;
pub mod types;
pub mod util;
pub mod webui;

/// SemVer of the daemon. Kept in sync with `module/module.prop` by `scripts/build.py`.
pub const VERSION: &str = "0.2.0-observe.1";

/// `versionCode` for KernelSU/Magisk. Bumped together with `VERSION`.
pub const VERSION_CODE: u32 = 200;

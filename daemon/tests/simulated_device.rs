//! End-to-end tests against a synthetic Xiaomi 15 tree.
//!
//! These cover the contract that matters most on a real device:
//!  * observe mode must not write a single byte;
//!  * dry run must report exactly what it would write;
//!  * enabled mode must write, verify, journal and roll back;
//!  * an external writer must win, and we must stop fighting it;
//!  * a crashed session must be replayed on the next start;
//!  * a crash loop must fall back to SAFE_MODE.

#![allow(clippy::field_reassign_with_default)]

mod common;

use common::Fixture;
use neurosched::config::Config;
use neurosched::engine::{status_json, Runtime};
use neurosched::storage::Paths;
use neurosched::sys::SysRoot;
use neurosched::types::{PolicyState, RunMode};

/// sysfs values are read back without the trailing newline the fixture writes.
fn val(fx: &Fixture, path: &str) -> String {
    fx.read(path).trim().to_string()
}

const UCLAMP_MIN: &str = "/sys/fs/cgroup/top-app/cpu.uclamp.min";
const UCLAMP_MAX: &str = "/sys/fs/cgroup/top-app/cpu.uclamp.max";

fn setup(name: &str, mode: RunMode) -> (Fixture, Runtime, Paths) {
    let fx = Fixture::xiaomi15(name);
    let mut cfg = Config::default();
    cfg.mode = mode;
    cfg.actuator.enabled = true;
    cfg.storage.data_dir = fx.root().join("data").to_string_lossy().to_string();
    cfg.api.enabled = false;
    let paths = Paths::new(fx.root().join("data"));
    let rt = Runtime::new(
        SysRoot::at(fx.root()),
        cfg,
        paths.clone(),
        0,
        "test-token".into(),
    )
    .expect("runtime");
    (fx, rt, paths)
}

fn make_hot(fx: &Fixture) {
    fx.set("/sys/class/thermal/thermal_zone0/temp", "85000\n");
    fx.set("/sys/class/thermal/thermal_zone1/temp", "84000\n");
}

#[test]
fn observe_mode_never_writes() {
    let (fx, mut rt, _p) = setup("observe", RunMode::ObserveOnly);
    make_hot(&fx);
    for t in [1_000, 120_000, 240_000] {
        rt.tick(t);
    }
    assert_eq!(val(&fx, UCLAMP_MIN), "0");
    assert_eq!(val(&fx, UCLAMP_MAX), "100");

    let mode = rt.shared().with(|d| d.mode);
    assert_eq!(mode, RunMode::ObserveOnly);
    let applied = rt.shared().with(|d| d.report.applied());
    assert_eq!(applied, 0);
    let journal = rt.shared().with(|d| d.journal.len());
    assert_eq!(journal, 0, "observe mode must not journal writes");
    rt.shutdown();
}

#[test]
fn stop_file_interrupts_a_long_sampling_wait() {
    use std::sync::{atomic::Ordering, mpsc};
    use std::time::{Duration, Instant};

    let fx = Fixture::xiaomi15("stop-during-idle");
    let mut cfg = Config::default();
    cfg.sample.interval_ms = 60_000;
    cfg.sample.screen_off_interval_ms = 120_000;
    cfg.api.enabled = false;
    let paths = Paths::new(fx.root().join("data"));
    let mut rt = Runtime::new(
        SysRoot::at(fx.root()),
        cfg,
        paths.clone(),
        0,
        "test-token".into(),
    )
    .unwrap();
    let shared = rt.shared();
    let stop = rt.stop_flag();
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        rt.run();
        let _ = tx.send(());
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while shared.with(|d| d.completed_ticks) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let sampled = shared.with(|d| d.completed_ticks) > 0;
    std::fs::write(paths.root.join("stop"), "1").unwrap();
    let stopped_promptly = rx.recv_timeout(Duration::from_secs(3)).is_ok();
    // Ensure a failing regression cannot leave a worker behind for two minutes.
    stop.store(true, Ordering::SeqCst);
    worker.join().unwrap();
    assert!(sampled, "runtime must reach its actual sampling loop");
    assert!(
        stopped_promptly,
        "stop file was delayed by the sampling interval"
    );
    assert!(!paths.journal().join("active").exists());
    assert_eq!(val(&fx, UCLAMP_MIN), "0");
    assert_eq!(val(&fx, UCLAMP_MAX), "100");
}

#[test]
fn detects_clusters_from_capability_not_index() {
    let (_fx, mut rt, _p) = setup("clusters", RunMode::ObserveOnly);
    rt.tick(120_000);
    let j = rt.shared().with(status_json);
    let clusters = j
        .get("cpu")
        .and_then(|c| c.get("clusters"))
        .map(|c| c.as_arr().to_vec())
        .unwrap_or_default();
    assert_eq!(clusters.len(), 2);
    assert_eq!(
        clusters[0].get("name").and_then(|v| v.as_str()),
        Some("prime")
    );
    assert_eq!(
        clusters[0].get("cpus").and_then(|v| v.as_str()),
        Some("6,7")
    );
    assert_eq!(
        clusters[1].get("name").and_then(|v| v.as_str()),
        Some("gold")
    );
    assert_eq!(
        clusters[0].get("governor").and_then(|v| v.as_str()),
        Some("schedutil")
    );
    rt.shutdown();
}

#[test]
fn hot_device_enters_thermal_guard() {
    let (fx, mut rt, _p) = setup("thermal", RunMode::ObserveOnly);
    make_hot(&fx);
    rt.tick(120_000);
    let state = rt.shared().with(|d| d.decision.state);
    assert_eq!(state, PolicyState::ThermalGuard);
    let profile = rt.shared().with(|d| d.decision.profile);
    assert!(profile.uclamp_max <= 640);
    assert_eq!(profile.uclamp_min, 0);
    rt.shutdown();
}

#[test]
fn dry_run_reports_but_does_not_write() {
    let (fx, mut rt, _p) = setup("dry", RunMode::DryRun);
    make_hot(&fx);
    rt.tick(120_000);
    let statuses: Vec<String> = rt
        .shared()
        .with(|d| d.report.results.iter().map(|r| r.status.clone()).collect());
    assert!(statuses.iter().any(|s| s == "planned"), "{statuses:?}");
    assert_eq!(val(&fx, UCLAMP_MAX), "100");
    rt.shutdown();
}

#[test]
fn enabled_mode_writes_verifies_and_restores_on_shutdown() {
    let (fx, mut rt, _p) = setup("enabled", RunMode::Enabled);
    make_hot(&fx);
    rt.tick(120_000);
    let applied = rt.shared().with(|d| d.report.applied());
    assert!(applied >= 1, "expected at least one applied write");
    assert_eq!(val(&fx, UCLAMP_MAX), "62.50");

    let journal = rt.shared().with(|d| d.journal.len());
    assert!(journal >= 1, "write must be journaled");

    // Clean shutdown restores the original value.
    rt.shutdown();
    assert_eq!(val(&fx, UCLAMP_MAX), "100");
}

#[test]
fn external_writer_wins_and_node_is_released() {
    let (fx, mut rt, _p) = setup("conflict", RunMode::Enabled);
    make_hot(&fx);
    rt.tick(120_000);
    assert_eq!(val(&fx, UCLAMP_MAX), "62.50");

    // Scene (or the framework) takes over the node.
    fx.clobber(UCLAMP_MAX, "50.00\n");
    rt.tick(240_000);

    let conflicts = rt.shared().with(|d| d.report.conflicts.clone());
    assert!(!conflicts.is_empty(), "conflict must be reported");
    assert_eq!(
        val(&fx, UCLAMP_MAX),
        "50.00",
        "must not clobber the other writer"
    );

    let released = rt.shared().with(|d| d.nodes.iter().any(|n| n.released));
    assert!(released, "node must be marked released");

    // Further ticks must not write it again.
    rt.tick(360_000);
    assert_eq!(val(&fx, UCLAMP_MAX), "50.00");
    rt.shutdown();
    assert_eq!(
        val(&fx, UCLAMP_MAX),
        "50.00",
        "shutdown must not fight either"
    );
}

#[test]
fn crashed_session_is_replayed_on_next_start() {
    let fx = Fixture::xiaomi15("recovery");
    let data = fx.root().join("data");
    let journal_dir = data.join("journal");
    std::fs::create_dir_all(&journal_dir).unwrap();
    // A previous run wrote 640 and died before closing its session.
    fx.set(UCLAMP_MAX, "62.50\n");
    let boot = fx
        .read("/proc/sys/kernel/random/boot_id")
        .trim()
        .to_string();
    std::fs::write(
        journal_dir.join("active"),
        format!("111-999\n1234\nboot_id={boot}\n"),
    )
    .unwrap();
    std::fs::write(journal_dir.join("111-999.boot_id"), format!("{boot}\n")).unwrap();
    std::fs::write(
        journal_dir.join("111-999.jsonl"),
        format!(
            "{{\"ts\":1,\"session\":\"111-999\",\"node\":\"{UCLAMP_MAX}\",\"old\":\"100\",\"new\":\"62.50\",\"state\":\"THERMAL_GUARD\",\"mode\":\"enabled\",\"result\":\"pending\"}}\n"
        ),
    )
    .unwrap();

    let mut cfg = Config::default();
    cfg.storage.data_dir = data.to_string_lossy().to_string();
    cfg.api.enabled = false;
    let rt = Runtime::new(
        SysRoot::at(fx.root()),
        cfg,
        Paths::new(&data),
        0,
        "t".into(),
    )
    .expect("runtime");

    assert_eq!(
        val(&fx, UCLAMP_MAX),
        "100",
        "pending write must be reverted"
    );
    let recovered = rt.shared().with(|d| d.recovered.clone());
    assert!(!recovered.is_empty());
    assert!(recovered[0].contains("restored 1"), "{recovered:?}");
}

#[test]
fn crash_loop_forces_safe_mode_and_no_writes() {
    let fx = Fixture::xiaomi15("safemode");
    let data = fx.root().join("data");
    std::fs::create_dir_all(&data).unwrap();
    // Two failed boots already, limit is 3 -> this start trips it.
    std::fs::write(
        data.join("boot.json"),
        "{\"boot_attempts\":2,\"consecutive_failures\":2,\"safe_mode_boots_left\":0,\"last_boot_ts\":1,\"healthy_marked\":false,\"last_version\":\"0.1.0\",\"last_mode\":\"enabled\"}",
    )
    .unwrap();
    make_hot(&fx);

    let mut cfg = Config::default();
    cfg.mode = RunMode::Enabled;
    cfg.storage.data_dir = data.to_string_lossy().to_string();
    cfg.api.enabled = false;
    let mut rt = Runtime::new(
        SysRoot::at(fx.root()),
        cfg,
        Paths::new(&data),
        0,
        "t".into(),
    )
    .expect("runtime");

    assert!(rt.shared().with(|d| d.safe_mode));
    rt.tick(120_000);
    assert_eq!(val(&fx, UCLAMP_MAX), "100", "safe mode must not write");
    assert_eq!(
        rt.shared().with(|d| d.decision.state),
        PolicyState::SafeMode
    );
    rt.shutdown();
}

#[test]
fn memory_and_thermal_telemetry_is_sane() {
    let (fx, mut rt, _p) = setup("telemetry", RunMode::ObserveOnly);
    fx.set("/sys/class/thermal/thermal_zone0/temp", "76000\n");
    rt.tick(120_000);
    let j = rt.shared().with(status_json);

    let mem = j.get("memory").expect("memory");
    assert_eq!(mem.get("total_mb").and_then(|v| v.as_u64()), Some(11264));
    assert_eq!(
        mem.get("mmd")
            .and_then(|m| m.get("present"))
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    let zram = mem.get("zram").map(|z| z.as_arr().len());
    assert_eq!(zram, Some(1));

    let psi = j.get("psi").and_then(|p| p.get("memory")).expect("psi");
    assert_eq!(psi.get("available").and_then(|v| v.as_bool()), Some(true));
    assert!(psi.get("some10").and_then(|v| v.as_f64()).unwrap() > 1.0);

    let thermal = j.get("thermal").expect("thermal");
    assert_eq!(
        thermal.get("status").and_then(|v| v.as_str()),
        Some("MODERATE")
    );
    // The charger zone is 36 C and the battery 32.1 C: neither may drive the CPU value.
    assert_eq!(thermal.get("cpu_c").and_then(|v| v.as_f64()), Some(76.0));

    let health = j.get("health").map(|h| h.as_arr()).unwrap_or(&[]);
    assert!(
        health
            .iter()
            .all(|c| c.get("ok").and_then(|v| v.as_bool()).unwrap_or(false)),
        "{health:?}"
    );
    rt.shutdown();
}

#[test]
fn companion_events_drive_foreground_and_launch_window() {
    let (fx, mut rt, paths) = setup("events", RunMode::ObserveOnly);
    // The companion app log is polled synchronously on snapshot.
    std::fs::write(paths.companion_log(), "com.tencent.mm\n").unwrap();
    rt.start_event_sources();
    rt.tick(120_000);

    let fg = rt.shared().with(|d| d.snapshot.apps.foreground.clone());
    assert_eq!(
        fg.as_deref(),
        Some("com.tencent.mm"),
        "companion log must be tailed"
    );
    let src = rt
        .shared()
        .with(|d| d.snapshot.apps.source.map(|s| s.as_str().to_string()));
    assert_eq!(src.as_deref(), Some("companion"));
    // The first tick after a foreground change is inside the launch boost window.
    assert_eq!(
        rt.shared().with(|d| d.decision.state),
        PolicyState::AppLaunch
    );
    // Launch boost has a hard expiry independent of generic state cooldown.
    rt.tick(125_000);
    assert_eq!(
        rt.shared().with(|d| d.decision.state),
        PolicyState::Foreground
    );
    rt.tick(132_000);
    assert_eq!(
        rt.shared().with(|d| d.decision.state),
        PolicyState::Foreground
    );
    let _ = fx;
    rt.shutdown();
}

#[test]
fn config_reload_applies_new_thresholds() {
    let (fx, mut rt, paths) = setup("reload", RunMode::ObserveOnly);
    let mut cfg = Config::default();
    cfg.policy.thermal.enter_light_c = 88.0;
    cfg.policy.thermal.enter_moderate_c = 92.0;
    cfg.policy.thermal.enter_severe_c = 96.0;
    cfg.policy.thermal.enter_critical_c = 100.0;
    cfg.storage.data_dir = paths.root.to_string_lossy().to_string();
    cfg.save(&paths.config()).expect("save config");

    make_hot(&fx); // 85 C
    rt.shared().request_reload();
    rt.tick(120_000);

    // 85 C is now below the new LIGHT threshold, so no thermal guard.
    assert_ne!(
        rt.shared().with(|d| d.decision.state),
        PolicyState::ThermalGuard
    );
    rt.shutdown();
}

// ---------------------------------------------------------------------------
// Thermal fail-closed
// ---------------------------------------------------------------------------

#[test]
fn missing_thermal_sensor_forces_observe_only() {
    let (fx, mut rt, _p) = setup("failclosed-none", RunMode::Enabled);
    std::fs::remove_dir_all(fx.path("/sys/class/thermal")).unwrap();
    rt.tick(120_000);

    assert!(rt.shared().with(|d| d.thermal_fail_closed));
    let reason = rt.shared().with(|d| d.fail_closed_reason.clone());
    assert!(reason.contains("thermal_service_unavailable"), "{reason}");
    assert_eq!(rt.shared().with(|d| d.mode), RunMode::ObserveOnly);
    // Config keeps the user's choice; only the runtime mode is downgraded.
    assert_eq!(rt.shared().with(|d| d.config.mode), RunMode::Enabled);
    assert_eq!(val(&fx, UCLAMP_MIN), "0");
    assert_eq!(rt.shared().with(|d| d.report.applied()), 0);
    rt.shutdown();
}

#[test]
fn ambiguous_thermal_mapping_forbids_boost() {
    let (fx, mut rt, _p) = setup("failclosed-ambiguous", RunMode::Enabled);
    // Vendor zone names we cannot classify: guessing would be worse than refusing.
    for i in 0..3 {
        fx.set(
            &format!("/sys/class/thermal/thermal_zone{i}/type"),
            "pm8350_tz\n",
        );
        fx.set(
            &format!("/sys/class/thermal/thermal_zone{i}/temp"),
            "48000\n",
        );
    }
    for i in 3..7 {
        std::fs::remove_dir_all(fx.path(&format!("/sys/class/thermal/thermal_zone{i}"))).unwrap();
    }
    rt.tick(120_000);

    assert!(rt.shared().with(|d| d.thermal_fail_closed));
    let trust = rt.shared().with(|d| d.snapshot.thermal.trust);
    assert_eq!(trust, neurosched::types::ThermalTrust::Ambiguous);
    assert_eq!(rt.shared().with(|d| d.decision.profile.uclamp_min), 0);
    assert_eq!(val(&fx, UCLAMP_MIN), "0");
    rt.shutdown();
}

#[test]
fn invalid_thermal_values_forbid_boost() {
    let (fx, mut rt, _p) = setup("failclosed-invalid", RunMode::Enabled);
    for i in 0..7 {
        fx.set(
            &format!("/sys/class/thermal/thermal_zone{i}/temp"),
            "999999\n",
        );
    }
    rt.tick(120_000);
    assert!(rt.shared().with(|d| d.thermal_fail_closed));
    assert_eq!(val(&fx, UCLAMP_MAX), "100");
    rt.shutdown();
}

#[test]
fn fail_closed_recovers_when_sensors_return() {
    let (fx, mut rt, _p) = setup("failclosed-recover", RunMode::Enabled);
    let saved: Vec<(String, String)> = (0..7)
        .map(|i| {
            (
                fx.read(&format!("/sys/class/thermal/thermal_zone{i}/type")),
                fx.read(&format!("/sys/class/thermal/thermal_zone{i}/temp")),
            )
        })
        .collect();
    std::fs::remove_dir_all(fx.path("/sys/class/thermal")).unwrap();
    rt.tick(120_000);
    assert!(rt.shared().with(|d| d.thermal_fail_closed));

    // Sensors come back.
    for (i, (t, v)) in saved.iter().enumerate() {
        fx.set(
            &format!("/sys/class/thermal/thermal_zone{i}/type"),
            t.trim(),
        );
        fx.set(
            &format!("/sys/class/thermal/thermal_zone{i}/temp"),
            v.trim(),
        );
    }
    rt.tick(130_000);
    assert!(!rt.shared().with(|d| d.thermal_fail_closed));
    assert_eq!(rt.shared().with(|d| d.mode), RunMode::Enabled);
    rt.shutdown();
}

#[test]
fn mode_downgrade_restores_and_stays_read_only() {
    let (fx, mut rt, paths) = setup("downgrade", RunMode::Enabled);
    make_hot(&fx);
    rt.tick(120_000);
    assert_eq!(val(&fx, UCLAMP_MAX), "62.50");
    let mut cfg = rt.shared().with(|d| d.config.clone());
    cfg.mode = RunMode::ObserveOnly;
    cfg.save(&paths.config()).unwrap();
    rt.shared().request_reload();
    rt.tick(125_000);
    assert_eq!(val(&fx, UCLAMP_MAX), "100");
    rt.tick(130_000);
    assert_eq!(val(&fx, UCLAMP_MAX), "100");
    rt.shutdown();
}

#[test]
fn manual_rollback_disables_future_execution_and_persists_consent() {
    let (fx, mut rt, paths) = setup("manual-rollback", RunMode::Enabled);
    make_hot(&fx);
    rt.tick(120_000);
    rt.shared()
        .request_action(neurosched::engine::Action::Rollback);
    rt.tick(125_000);
    assert_eq!(val(&fx, UCLAMP_MAX), "100");
    assert_eq!(rt.shared().with(|d| d.mode), RunMode::ObserveOnly);
    assert!(!Config::load(&paths.config()).unwrap().actuator.enabled);
    rt.tick(130_000);
    assert_eq!(val(&fx, UCLAMP_MAX), "100");
    rt.shutdown();
}

#[test]
fn loss_of_thermal_trust_restores_existing_cap() {
    let (fx, mut rt, _paths) = setup("thermal-loss-after-write", RunMode::Enabled);
    make_hot(&fx);
    rt.tick(120_000);
    assert_eq!(val(&fx, UCLAMP_MAX), "62.50");
    std::fs::remove_dir_all(fx.path("/sys/class/thermal")).unwrap();
    rt.tick(125_000);
    assert!(rt.shared().with(|d| d.thermal_fail_closed));
    assert_eq!(val(&fx, UCLAMP_MAX), "100");
    rt.shutdown();
}

#[test]
fn actuator_opt_out_hot_reload_restores_and_blocks_next_tick() {
    let (fx, mut rt, paths) = setup("actuator-opt-out", RunMode::Enabled);
    make_hot(&fx);
    rt.tick(120_000);
    let mut cfg = rt.shared().with(|d| d.config.clone());
    cfg.actuator.enabled = false;
    cfg.save(&paths.config()).unwrap();
    rt.shared().request_reload();
    rt.tick(121_000);
    assert_eq!(val(&fx, UCLAMP_MAX), "100");
    assert_eq!(rt.shared().with(|d| d.mode), RunMode::ObserveOnly);
    rt.shutdown();
}

#[test]
fn second_runtime_cannot_claim_live_journal() {
    let (fx, mut first, paths) = setup("single-instance", RunMode::ObserveOnly);
    let active = std::fs::read(paths.journal().join("active")).unwrap();
    let result = Runtime::new(
        SysRoot::at(fx.root()),
        Config::default(),
        paths.clone(),
        100,
        "t".into(),
    );
    assert!(result.is_err());
    assert_eq!(
        std::fs::read(paths.journal().join("active")).unwrap(),
        active
    );
    first.shutdown();
}

#[test]
fn failed_recovery_preserves_previous_active_session() {
    let fx = Fixture::xiaomi15("failed-recovery-evidence");
    let paths = Paths::new(fx.root().join("data"));
    paths.ensure().unwrap();
    let journal = neurosched::safety::transaction::Journal::for_root(
        paths.journal(),
        "crashed".into(),
        &SysRoot::at(fx.root()),
    )
    .unwrap();
    journal.begin().unwrap();
    journal
        .append(&neurosched::safety::transaction::JournalEntry {
            ts_ms: 1,
            session: "crashed".into(),
            node: UCLAMP_MAX.into(),
            old: "100".into(),
            new: "62.50".into(),
            state: "THERMAL_GUARD".into(),
            mode: "enabled".into(),
            result: "pending".into(),
        })
        .unwrap();
    fx.set(UCLAMP_MAX, "62.50");
    let mut faults = std::collections::BTreeMap::new();
    faults.insert(UCLAMP_MAX.into(), neurosched::sys::Fault::WriteFail);
    let result = Runtime::new(
        SysRoot::at(fx.root()).with_faults(faults),
        Config::default(),
        paths.clone(),
        2,
        "t".into(),
    );
    assert!(result.is_err());
    assert_eq!(
        std::fs::read_to_string(paths.journal().join("active"))
            .unwrap()
            .lines()
            .next(),
        Some("crashed")
    );
    assert_eq!(val(&fx, UCLAMP_MAX), "62.50");
}

#[test]
fn inspection_cli_preserves_live_state_and_creates_no_data() {
    let (fx, mut rt, paths) = setup("cli-inspection", RunMode::Enabled);
    make_hot(&fx);
    rt.tick(120_000);
    let active = std::fs::read(paths.journal().join("active")).unwrap();
    for flag in ["--status", "--once", "--self-test"] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_neuroschedd"))
            .arg("--fixture")
            .arg(fx.root())
            .arg("--data-dir")
            .arg(&paths.root)
            .arg(flag)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{flag}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read(paths.journal().join("active")).unwrap(),
            active
        );
        assert_eq!(val(&fx, UCLAMP_MAX), "62.50");
    }
    let unused = fx.root().join("never-created");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_neuroschedd"))
        .arg("--fixture")
        .arg(fx.root())
        .arg("--data-dir")
        .arg(&unused)
        .arg("--self-test")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!unused.exists());
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_neuroschedd"))
        .arg("--fixture")
        .arg(fx.root())
        .arg("--data-dir")
        .arg(&paths.root)
        .arg("--restore-all")
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "restore must refuse a live owner's lock"
    );
    rt.shutdown();
}

#[test]
fn runtime_history_uses_wall_clock_while_state_uses_monotonic_clock() {
    let (_fx, mut rt, _paths) = setup("two-clocks", RunMode::ObserveOnly);
    let epoch = 1_798_761_600_000;
    rt.tick_at(120_000, epoch);
    rt.shared().with(|d| {
        assert_eq!(d.snapshot.ts_ms, 120_000);
        assert_eq!(d.snapshot.wall_ts_ms, epoch);
        assert_eq!(d.history.back().unwrap().ts_ms, epoch);
        assert!(!d.history.back().unwrap().to_csv().starts_with("1970-"));
    });
    rt.shutdown();
}

#[test]
fn settled_rollback_is_not_replayed_after_an_external_write() {
    let (fx, mut rt, paths) = setup("settled-runtime", RunMode::Enabled);
    make_hot(&fx);
    rt.tick(120_000);
    rt.shared()
        .request_action(neurosched::engine::Action::Rollback);
    rt.tick(125_000);
    assert_eq!(val(&fx, UCLAMP_MAX), "100");
    fx.set(UCLAMP_MAX, "62.50");
    let cfg = rt.shared().with(|d| d.config.clone());
    drop(rt);
    let mut recovered =
        Runtime::new(SysRoot::at(fx.root()), cfg, paths, 0, "test-token".into()).unwrap();
    assert_eq!(val(&fx, UCLAMP_MAX), "62.50");
    recovered.shutdown();
}

#[test]
fn restore_conflict_remains_released_across_config_reload() {
    let (fx, mut rt, paths) = setup("restore-release", RunMode::Enabled);
    make_hot(&fx);
    rt.tick(120_000);
    fx.set(UCLAMP_MAX, "50.00");
    rt.shared()
        .with(|d| d.config.save(&paths.config()).unwrap());
    rt.shared().request_reload();
    rt.tick(125_000);
    assert_eq!(val(&fx, UCLAMP_MAX), "50.00");
    assert!(rt
        .shared()
        .with(|d| d.nodes.iter().any(|n| n.node == UCLAMP_MAX && n.released)));
    rt.shutdown();
    assert_eq!(val(&fx, UCLAMP_MAX), "50.00");
}

#[test]
fn custom_config_path_is_used_for_reload_and_rollback() {
    let fx = Fixture::xiaomi15("custom-config");
    let custom = fx.root().join("settings/alternate.json");
    let paths = Paths::new(fx.root().join("data")).with_config(&custom);
    let mut cfg = Config::default();
    cfg.mode = RunMode::DryRun;
    cfg.save(&custom).unwrap();
    let mut rt = Runtime::new(
        SysRoot::at(fx.root()),
        cfg.clone(),
        paths.clone(),
        0,
        "test-token".into(),
    )
    .unwrap();
    rt.tick(120_000);
    assert_eq!(rt.shared().with(|d| d.mode), RunMode::DryRun);
    assert!(!rt.shared().with(|d| d.config.actuator.enabled));
    cfg.policy.thermal.enter_moderate_c = 72.0;
    cfg.save(&custom).unwrap();
    rt.shared().request_reload();
    rt.tick(125_000);
    assert_eq!(
        rt.shared()
            .with(|d| d.config.policy.thermal.enter_moderate_c),
        72.0
    );
    rt.shared()
        .request_action(neurosched::engine::Action::Rollback);
    rt.tick(130_000);
    assert_eq!(Config::load(&custom).unwrap().mode, RunMode::ObserveOnly);
    assert!(!paths.root.join("config.json").exists());
    rt.shutdown();
}

#[test]
fn capability_reads_current_uclamp_and_marks_unreadable_values_unknown() {
    let (fx, mut rt, _paths) = setup("live-capability", RunMode::ObserveOnly);
    fx.set(UCLAMP_MAX, "50.00");
    rt.tick(120_000);
    assert_eq!(
        rt.shared().with(|d| d
            .capability
            .get_path("uclamp.current_max")
            .and_then(|j| j.as_f64())),
        Some(50.0)
    );
    std::fs::remove_file(fx.path(UCLAMP_MAX)).unwrap();
    rt.tick(125_000);
    assert_eq!(
        rt.shared()
            .with(|d| d.capability.get_path("uclamp.current_max").cloned()),
        Some(neurosched::json::Json::Null)
    );
    rt.shutdown();
}

#[test]
fn killed_daemon_releases_lock_and_cli_recovers_the_durable_journal() {
    let fx = Fixture::xiaomi15("process-crash");
    make_hot(&fx);
    let paths = Paths::new(fx.root().join("data"));
    let mut cfg = Config::default();
    cfg.mode = RunMode::Enabled;
    cfg.actuator.enabled = true;
    cfg.policy.boot_grace_ms = 0;
    cfg.sample.interval_ms = 250;
    cfg.api.enabled = false;
    cfg.save(&paths.config()).unwrap();
    let binary = env!("CARGO_BIN_EXE_neuroschedd");
    let mut child = std::process::Command::new(binary)
        .arg("--fixture")
        .arg(fx.root())
        .arg("--data-dir")
        .arg(&paths.root)
        .arg("--no-api")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while val(&fx, UCLAMP_MAX) != "62.50" && std::time::Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let wrote = val(&fx, UCLAMP_MAX) == "62.50";
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
    }
    child.wait().unwrap();
    assert!(
        wrote,
        "fixture daemon must reach the real write/journal entry point before termination"
    );
    assert!(paths.journal().join("active").exists());
    let output = std::process::Command::new(binary)
        .arg("--fixture")
        .arg(fx.root())
        .arg("--data-dir")
        .arg(&paths.root)
        .arg("--restore-all")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(val(&fx, UCLAMP_MAX), "100");
    assert!(!paths.journal().join("active").exists());
}

#[test]
fn failed_restore_checkpoint_keeps_the_session_active_and_read_only() {
    let (fx, mut rt, paths) = setup("checkpoint-failure", RunMode::Enabled);
    make_hot(&fx);
    rt.tick(120_000);
    let active = std::fs::read_to_string(paths.journal().join("active")).unwrap();
    let session = active.lines().next().unwrap();
    let journal_file = paths.journal().join(format!("{session}.jsonl"));
    std::fs::rename(&journal_file, journal_file.with_extension("saved")).unwrap();
    std::fs::create_dir(&journal_file).unwrap();
    rt.shared()
        .request_action(neurosched::engine::Action::Rollback);
    rt.tick(125_000);
    assert_eq!(val(&fx, UCLAMP_MAX), "100");
    assert!(rt.shared().with(|d| d.safe_mode));
    assert_eq!(rt.shared().with(|d| d.mode), RunMode::ObserveOnly);
    assert_eq!(
        rt.shared().with(|d| d
            .capability
            .get("recovery_required")
            .and_then(|j| j.as_bool())),
        Some(true)
    );
    rt.shutdown();
    assert!(paths.journal().join("active").exists());
}

#[test]
fn new_boot_archives_old_journal_without_replaying_even_matching_node_values() {
    let (fx, mut first, paths) = setup("boot-boundary-runtime", RunMode::Enabled);
    make_hot(&fx);
    first.tick(120_000);
    assert_eq!(val(&fx, UCLAMP_MAX), "62.50");
    let marker = std::fs::read_to_string(paths.journal().join("active")).unwrap();
    let old_session = marker.lines().next().unwrap().to_string();
    drop(first); // mimic abrupt loss, intentionally no shutdown restoration
    fx.set(
        "/proc/sys/kernel/random/boot_id",
        "22222222-2222-4222-8222-222222222222\n",
    );
    let mut next = Runtime::new(
        SysRoot::at(fx.root()),
        Config::default(),
        paths.clone(),
        0,
        "t".into(),
    )
    .unwrap();
    next.tick(120_000);
    assert_eq!(val(&fx, UCLAMP_MAX), "62.50");
    assert!(paths
        .journal()
        .join(format!("{old_session}.previous-boot"))
        .exists());
    assert!(paths
        .journal()
        .join(format!("{old_session}.jsonl"))
        .exists());
    assert!(next.shared().with(|d| d.safe_mode));
    assert_eq!(next.shared().with(|d| d.mode), RunMode::ObserveOnly);
    next.shutdown();
}

#[test]
fn empty_previous_boot_journal_does_not_force_safe_mode() {
    let (fx, mut first, paths) = setup("boot-boundary-empty", RunMode::ObserveOnly);
    first.tick(120_000);
    let marker = std::fs::read_to_string(paths.journal().join("active")).unwrap();
    let old_session = marker.lines().next().unwrap().to_string();
    drop(first); // reboot before shutdown could remove the empty active marker
    fx.set(
        "/proc/sys/kernel/random/boot_id",
        "22222222-2222-4222-8222-222222222222\n",
    );
    let mut next = Runtime::new(
        SysRoot::at(fx.root()),
        Config::default(),
        paths.clone(),
        0,
        "t".into(),
    )
    .unwrap();
    next.tick(120_000);
    assert!(!paths
        .journal()
        .join(format!("{old_session}.previous-boot"))
        .exists());
    assert!(!next.shared().with(|d| d.safe_mode));
    assert_ne!(
        next.shared().with(|d| d.decision.state),
        PolicyState::SafeMode
    );

    // Even a single byte of unclosed journal intent across boots still forces SAFE_MODE.
    let marker2 = std::fs::read_to_string(paths.journal().join("active")).unwrap();
    let second_session = marker2.lines().next().unwrap().to_string();
    drop(next);
    std::fs::write(
        paths.journal().join(format!("{second_session}.jsonl")),
        "{unfinished",
    )
    .unwrap();
    fx.set(
        "/proc/sys/kernel/random/boot_id",
        "33333333-3333-4333-8333-333333333333\n",
    );
    let mut third = Runtime::new(
        SysRoot::at(fx.root()),
        Config::default(),
        paths.clone(),
        0,
        "t".into(),
    )
    .unwrap();
    third.tick(120_000);
    assert!(paths
        .journal()
        .join(format!("{second_session}.previous-boot"))
        .exists());
    assert!(third.shared().with(|d| d.safe_mode));
    assert_eq!(
        third.shared().with(|d| d.decision.state),
        PolicyState::SafeMode
    );
    third.shutdown();
}

#[test]
fn manual_restore_on_new_boot_archives_without_writing() {
    let (fx, mut first, paths) = setup("boot-boundary-cli", RunMode::Enabled);
    make_hot(&fx);
    first.tick(120_000);
    drop(first);
    fx.set(
        "/proc/sys/kernel/random/boot_id",
        "22222222-2222-4222-8222-222222222222\n",
    );
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_neuroschedd"))
        .args([
            "--fixture",
            fx.root().to_str().unwrap(),
            "--data-dir",
            paths.root.to_str().unwrap(),
            "--restore-all",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("no kernel values replayed"));
    assert_eq!(val(&fx, UCLAMP_MAX), "62.50");
    assert!(!paths.journal().join("active").exists());
}

#[test]
fn diagnostics_export_omits_application_and_credential_context() {
    let (_fx, mut rt, _paths) = setup("diagnostic-redaction", RunMode::ObserveOnly);
    rt.tick(1_000);
    rt.shared().mutate(|d| {
        d.snapshot.apps.foreground = Some("com.private.application".into());
        d.device.fingerprint = "PRIVATE_FINGERPRINT".into();
        d.safe_reason = "PRIVATE_RECOVERY_TEXT".into();
        d.recovered.push("PRIVATE_BOOT_ID".into());
        d.config.storage.data_dir = "PRIVATE_DATA_PATH".into();
        d.snapshot.health.collectors[0].note = "PRIVATE_NOTE".into();
    });
    let report = rt.shared().with(neurosched::engine::diagnostics_json);
    let text = report.to_string();
    for private in ["com.private.application", "PRIVATE_", "test-token"] {
        assert!(!text.contains(private), "export leaked {private}");
    }
    assert_eq!(
        report
            .get_path("daemon.completed_ticks")
            .and_then(|j| j.as_u64()),
        Some(1)
    );
    assert!(report.get("sampling").is_some());
    assert_eq!(
        report.get("ACTUATOR_ENABLED").and_then(|j| j.as_bool()),
        Some(false)
    );
    rt.shutdown();
}

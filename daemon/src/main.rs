//! CLI entry point.
//!
//! `neuroschedd` is intentionally small: parse a handful of flags, build a runtime and
//! either run the control loop or print one document. Everything testable lives in the
//! library.

use neurosched::api;
use neurosched::config::Config;
use neurosched::engine::{resolve_paths, Runtime};
use neurosched::json::Json;
use neurosched::storage::ensure_token;
use neurosched::sys::SysRoot;
use neurosched::types::RunMode;
use std::path::PathBuf;
use std::process::exit;

const HELP: &str = r#"NeuroSched - telemetry driven resource policy for Snapdragon 8 Elite

USAGE:
    neuroschedd [OPTIONS]

OPTIONS:
    --config <PATH>      Config file (default: <data-dir>/config.json)
    --data-dir <PATH>    Data directory (default: /data/adb/neurosched)
    --mode <MODE>        Config override: observe_only | dry_run | enabled
    --port <PORT>        HTTP port override, 1..65535
    --no-api             Do not start the local HTTP server
    --once               Read-only kernel snapshot, print JSON, exit
    --status             Alias for --once
    --self-test          Read-only capability report, exit nonzero if degraded
    --print-default-config Print safe default JSON without filesystem changes
    --restore-all        Restore every node we own, then exit
    --fixture <PATH>     Use a synthetic root instead of / (testing)
    --stop               Ask a running daemon to stop cleanly
    --version            Print version
    --help               Print this help

NOTES:
    Choose one operation flag. Inspection commands leave configuration, credentials,
    telemetry, models and journals untouched. Live Daemon state is in the WebUI.
    The default mode is observe_only. ACTUATOR_ENABLED=false prevents production
    kernel writes even when a legacy configuration requests enabled mode.
"#;

fn main() {
    // With `panic = "abort"` the hook still runs before the abort; this is the only
    // way a panic becomes visible in the captured stdout instead of a silent exit code.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("PANIC: {info}");
    }));

    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = parse_args(&args).unwrap_or_else(|error| {
        eprintln!("{error}\n\n{HELP}");
        exit(2);
    });

    match opts.command {
        Command::Help => {
            println!("{HELP}");
            return;
        }
        Command::Version => {
            println!(
                "neuroschedd {} ({})",
                neurosched::VERSION,
                neurosched::VERSION_CODE
            );
            return;
        }
        Command::PrintDefaultConfig => {
            println!("{}", Config::default().to_json().to_string_pretty());
            return;
        }
        _ => {}
    }

    if opts.command == Command::Stop {
        // Cooperative shutdown: the daemon watches for this file and restores nodes.
        let dir = opts
            .data_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("/data/adb/neurosched"));
        if let Err(err) = std::fs::write(dir.join("stop"), "1") {
            eprintln!("stop request failed: {err}");
            exit(1);
        }
        println!("stop requested ({})", dir.join("stop").display());
        return;
    }

    let mut root = match &opts.fixture {
        Some(p) => SysRoot::at(p.clone()),
        None => SysRoot::system(),
    };
    if matches!(opts.command, Command::Inspect | Command::SelfTest) {
        root = root.read_only();
    }

    let default_dir = opts
        .data_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("/data/adb/neurosched"));
    let config_path = opts
        .config
        .clone()
        .unwrap_or_else(|| default_dir.join("config.json"));

    let inspecting = matches!(
        opts.command,
        Command::Inspect | Command::SelfTest | Command::RestoreAll
    );
    let (mut cfg, warning) = if inspecting {
        match Config::load(&config_path) {
            Ok(cfg) => (cfg, None),
            Err(err) => (Config::default(), config_path.exists().then_some(err)),
        }
    } else {
        Config::load_or_recover(&config_path)
    };
    if let Some(w) = warning {
        eprintln!("config: {w}");
    }
    if let Some(mode) = opts.mode {
        cfg.mode = mode;
    }
    if let Some(port) = opts.port {
        cfg.api.port = port;
    }
    cfg.storage.data_dir = default_dir.to_string_lossy().to_string();

    let paths = resolve_paths(Some(default_dir.clone()), &cfg).with_config(config_path.clone());
    if opts.command == Command::SelfTest {
        let (report, failures) = self_test_report(&root, &cfg, &paths);
        println!("{}", report.to_string_pretty());
        exit(failures);
    }
    if opts.command == Command::Inspect {
        let mut collectors = neurosched::collector::Collectors::new(&cfg);
        let now = neurosched::util::mono_ms();
        let wall_ms = neurosched::util::now_ms();
        let mut snap = collectors.sample(&root, now);
        snap.wall_ts_ms = wall_ms;
        let is_launcher = snap
            .apps
            .foreground
            .as_deref()
            .is_some_and(|fg| fg == "-" || cfg.apps.launchers.iter().any(|l| l == fg));
        let mut engine = neurosched::policy::Engine::new(
            cfg.policy.clone(),
            cfg.actuator.profiles.clone(),
            cfg.apps.launchers.clone(),
            now,
        );
        let decision = engine.evaluate(&snap, now);
        let mut learner =
            neurosched::learner::Learner::load(&paths.sched_model(), &paths.learner_state());
        let learner_snap = learner.on_tick(now, wall_ms, &snap, is_launcher, &decision);
        let actuator = neurosched::actuator::Actuator::new(
            root.clone(),
            &cfg.actuator,
            RunMode::ObserveOnly,
            neurosched::safety::transaction::Journal::disabled(),
        );
        let data = neurosched::engine::Data {
            snapshot: snap,
            decision,
            learner: learner_snap,
            configured_mode: cfg.mode,
            config: cfg,
            capability: actuator.capability_json(),
            device: neurosched::engine::DeviceInfo::probe(&root),
            ..Default::default()
        };
        let mut out = neurosched::engine::status_json(&data);
        out.set("inspection", Json::Bool(true));
        out.set("config_path", Json::str(config_path.to_string_lossy()));
        out.set("data_dir", Json::str(paths.root.to_string_lossy()));
        println!("{}", out.to_string_pretty());
        return;
    }
    if opts.command == Command::RestoreAll {
        if !paths.root.exists() {
            println!("no data directory to recover");
            return;
        }
        let _lock =
            neurosched::safety::lock::RuntimeLock::acquire(&paths.root).unwrap_or_else(|err| {
                eprintln!("restore refused: daemon lock unavailable ({err})");
                exit(1);
            });
        use neurosched::safety::transaction::{Journal, PendingRecovery};
        let journal =
            Journal::for_root(paths.journal(), "restore".into(), &root).unwrap_or_else(|err| {
                eprintln!("restore refused: {err}");
                exit(1);
            });
        let pending = journal.pending_recovery().unwrap_or_else(|err| {
            eprintln!("restore refused: unreadable journal ({err})");
            exit(1);
        });
        if let Some(PendingRecovery::Replay { entries, .. }) = pending {
            let report = neurosched::actuator::rollback::replay(&root, &entries);
            println!("{}", report.to_json().to_string_pretty());
            if report.failed > 0 {
                exit(1);
            }
            if let Err(err) = journal.end() {
                eprintln!("journal close failed: {err}");
                exit(1);
            }
        } else if let Some(PendingRecovery::PreviousBoot { session, empty, .. }) = pending {
            if empty {
                if let Err(err) = journal.end() {
                    eprintln!("journal close failed: {err}");
                    exit(1);
                }
                println!("empty previous boot journal closed; no kernel values replayed");
            } else {
                if let Err(err) = journal.archive_previous_boot(&session) {
                    eprintln!("archive failed: {err}");
                    exit(1);
                }
                println!("previous boot journal archived; no kernel values replayed");
            }
        }
        return;
    }

    if !config_path.exists() {
        if let Err(e) = cfg.save(&config_path) {
            eprintln!("warning: could not write default config: {e}");
        }
    }

    if let Err(e) = paths.ensure() {
        eprintln!("warning: cannot prepare {}: {e}", paths.root.display());
    }
    let token = match ensure_token(&paths.token()) {
        Ok(t) => t,
        Err(e) => {
            // A crash loop here would be worse than losing the local console: keep
            // monitoring, disable the API, and say so.
            eprintln!("warning: cannot create API token ({e}); local API disabled");
            String::new()
        }
    };

    let boot_ms = neurosched::util::mono_ms();
    let mut runtime = match Runtime::new(
        root.clone(),
        cfg.clone(),
        paths.clone(),
        boot_ms,
        token.clone(),
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fatal: cannot start runtime: {e}");
            exit(1);
        }
    };

    // Long running daemon.
    #[cfg(unix)]
    install_signal_handlers(&runtime);
    if cfg.api.enabled && !opts.no_api && !token.is_empty() {
        let shared = runtime.shared();
        match api::serve(shared, &cfg.api.bind, cfg.api.port) {
            Ok(_) => {}
            Err(e) => eprintln!("warning: API disabled ({e})"),
        }
    }
    runtime.run();
}

/// SIGTERM/SIGINT stop the daemon through the same flag the loop already polls, so the
/// shutdown path (restore nodes, close journal) always runs. SIGHUP reloads the config.
#[cfg(unix)]
fn install_signal_handlers(runtime: &Runtime) {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    use signal_hook::{flag, iterator::Signals};

    let stop = runtime.stop_flag();
    for sig in [SIGTERM, SIGINT] {
        if let Err(e) = flag::register(sig, stop.clone()) {
            eprintln!("warning: cannot handle signal {sig}: {e}");
        }
    }

    let shared = runtime.shared();
    std::thread::spawn(move || {
        let Ok(mut signals) = Signals::new([SIGHUP]) else {
            return;
        };
        for _ in signals.forever() {
            shared
                .logger
                .info("signal", "SIGHUP received, reloading config");
            shared.request_reload();
        }
    });
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Command {
    #[default]
    Daemon,
    Inspect,
    SelfTest,
    PrintDefaultConfig,
    RestoreAll,
    Stop,
    Help,
    Version,
}

#[derive(Debug, Default)]
struct Opts {
    config: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    mode: Option<RunMode>,
    port: Option<u16>,
    fixture: Option<PathBuf>,
    no_api: bool,
    command: Command,
}

fn parse_args(args: &[String]) -> Result<Opts, String> {
    let mut opts = Opts::default();
    let mut args = args.iter();
    while let Some(option) = args.next() {
        match option.as_str() {
            "--config" => opts.config = Some(PathBuf::from(option_value(&mut args, option)?)),
            "--data-dir" => opts.data_dir = Some(PathBuf::from(option_value(&mut args, option)?)),
            "--fixture" => opts.fixture = Some(PathBuf::from(option_value(&mut args, option)?)),
            "--mode" => {
                let value = option_value(&mut args, option)?;
                opts.mode =
                    Some(RunMode::parse(value).ok_or_else(|| format!("invalid mode {value}"))?);
            }
            "--port" => {
                let value = option_value(&mut args, option)?;
                opts.port = Some(
                    value
                        .parse::<u16>()
                        .ok()
                        .filter(|port| *port > 0)
                        .ok_or_else(|| format!("invalid port {value}. Expected 1..65535"))?,
                );
            }
            "--no-api" => opts.no_api = true,
            "--once" | "--status" => select_command(&mut opts.command, Command::Inspect, option)?,
            "--self-test" => select_command(&mut opts.command, Command::SelfTest, option)?,
            "--print-default-config" => {
                select_command(&mut opts.command, Command::PrintDefaultConfig, option)?
            }
            "--restore-all" => select_command(&mut opts.command, Command::RestoreAll, option)?,
            "--stop" => select_command(&mut opts.command, Command::Stop, option)?,
            "--help" | "-h" => select_command(&mut opts.command, Command::Help, option)?,
            "--version" | "-V" => select_command(&mut opts.command, Command::Version, option)?,
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(opts)
}

fn option_value<'a>(
    args: &mut std::slice::Iter<'a, String>,
    option: &str,
) -> Result<&'a str, String> {
    args.next()
        .filter(|value| !value.is_empty() && !value.starts_with('-'))
        .map(String::as_str)
        .ok_or_else(|| format!("{option} requires a value"))
}

fn select_command(current: &mut Command, requested: Command, option: &str) -> Result<(), String> {
    if *current != Command::Daemon && *current != requested {
        return Err(format!("conflicting commands {current:?} and {option}"));
    }
    *current = requested;
    Ok(())
}

/// Read-only capability report. Safe to run on a live device: it performs no writes.
fn self_test_report(
    root: &SysRoot,
    cfg: &Config,
    paths: &neurosched::storage::Paths,
) -> (Json, i32) {
    use neurosched::actuator::uclamp;
    use neurosched::collector::{battery, cpu, memory, pressure, process, thermal};

    let mut failures = 0;
    let mut j = Json::obj();
    j.set("version", Json::str(neurosched::VERSION.to_string()));
    j.set(
        "root",
        Json::str(if root.is_system() {
            "/".to_string()
        } else {
            "fixture".to_string()
        }),
    );
    j.set("inspection", Json::Bool(true));
    j.set(
        "ACTUATOR_ENABLED",
        Json::Bool(neurosched::sys::ACTUATOR_ENABLED),
    );

    // CPU
    let mut cpu_c = cpu::CpuCollector::new();
    let snap = cpu_c.collect(root, 1_000);
    let mut c = Json::obj();
    c.set("clusters", Json::num(snap.clusters.len() as f64));
    c.set("present", Json::str(fmt_list(&snap.present)));
    c.set("uclamp", Json::Bool(snap.uclamp_supported));
    c.set("walt", Json::Bool(snap.walt_present));
    c.set(
        "policies",
        Json::Arr(
            snap.clusters
                .iter()
                .map(|cl| {
                    let mut o = Json::obj();
                    o.set("name", Json::str(cl.name.clone()));
                    o.set("cpus", Json::str(fmt_list(&cl.cpus)));
                    o.set("governor", Json::str(cl.governor.clone()));
                    o.set("max_mhz", Json::num(cl.cpuinfo_max_khz as f64 / 1000.0));
                    o.set("capacity", Json::num(cl.capacity.unwrap_or(0) as f64));
                    o
                })
                .collect(),
        ),
    );
    if snap.clusters.is_empty() {
        failures += 1;
        c.set("error", Json::str("no cpufreq policy readable"));
    }
    j.set("cpu", c);

    // Memory + PSI
    let mut mc = memory::MemoryCollector::new();
    let mem = mc.collect(root, 1_000);
    let psi = pressure::collect(root);
    let mut m = Json::obj();
    m.set("total_mb", Json::num(mem.total_kb / 1024.0));
    m.set("available_mb", Json::num(mem.available_kb / 1024.0));
    m.set("zram_disks", Json::num(mem.zram.len() as f64));
    m.set("swap_devices", Json::str(mem.swap_devices.join(" ")));
    m.set("psi_memory", Json::Bool(psi.memory.available));
    m.set("psi_cpu", Json::Bool(psi.cpu.available));
    if !mem.meminfo_ok {
        failures += 1;
        m.set("error", Json::str("/proc/meminfo unreadable"));
    }
    j.set("memory", m);

    // Thermal
    let mut tc = thermal::ThermalCollector::new(cfg.policy.thermal.clone(), 0);
    let th = tc.collect(root, 1_000);
    let mut t = Json::obj();
    t.set("zones", Json::num(th.zones.len() as f64));
    t.set("cpu_c", th.cpu_max_c.map(Json::Num).unwrap_or(Json::Null));
    t.set(
        "level",
        th.level
            .map(|level| Json::str(level.as_str()))
            .unwrap_or(Json::Null),
    );
    t.set("trust", Json::str(th.trust.as_str()));
    t.set("detail", Json::str(th.detail.clone()));
    t.set(
        "age_ms",
        if th.age_ms == u64::MAX {
            Json::Null
        } else {
            Json::num(th.age_ms as f64)
        },
    );
    t.set(
        "mapping",
        Json::Arr(
            th.zones
                .iter()
                .map(|z| {
                    let mut o = Json::obj();
                    o.set("name", Json::str(z.name.clone()));
                    o.set("kind", Json::str(z.kind.as_str()));
                    o.set("temp_c", Json::num(z.temp_c));
                    o
                })
                .collect(),
        ),
    );
    if !th.trust.is_trusted() {
        failures += 1;
        t.set("warning", Json::str("CPU thermal telemetry is untrusted"));
    }
    j.set("thermal", t);

    let bat = battery::collect(root);
    let mut b = Json::obj();
    b.set("present", Json::Bool(bat.present));
    b.set("pct", bat.capacity_pct.map(Json::Num).unwrap_or(Json::Null));
    b.set("temp_c", bat.temp_c.map(Json::Num).unwrap_or(Json::Null));
    b.set("status", Json::str(bat.status.clone()));
    j.set("battery", b);

    let mmd = process::detect_mmd(root, None);
    let mut md = Json::obj();
    md.set("present", Json::Bool(mmd.present));
    md.set("pid", Json::num(mmd.pid.unwrap_or(0) as f64));
    j.set("mmd", md);

    // Actuator capability (read-only)
    let u = uclamp::discover(root);
    let mut a = Json::obj();
    a.set("mode", Json::str(RunMode::ObserveOnly.as_str()));
    a.set("configured_mode", Json::str(cfg.mode.as_str()));
    a.set(
        "ACTUATOR_ENABLED",
        Json::Bool(neurosched::sys::ACTUATOR_ENABLED),
    );
    a.set("writes_allowed", Json::Bool(false));
    a.set("write_probe_performed", Json::Bool(false));
    a.set(
        "mountinfo_readable",
        Json::Bool(root.read_opt("/proc/self/mountinfo").is_some()),
    );
    a.set(
        "uclamp",
        match &u {
            Some(t) => {
                let mut o = Json::obj();
                o.set("dir", Json::str(t.dir.clone()));
                o.set("flavour", Json::str(t.flavour.clone()));
                o.set("min_path", Json::str(t.min_path.clone()));
                o.set("max_path", Json::str(t.max_path.clone()));
                o.set(
                    "raw_min",
                    root.read_trim(&t.min_path)
                        .map(Json::Str)
                        .unwrap_or(Json::Null),
                );
                o.set(
                    "raw_max",
                    root.read_trim(&t.max_path)
                        .map(Json::Str)
                        .unwrap_or(Json::Null),
                );
                o.set("current_min", Json::num(t.current_min));
                o.set("current_max", Json::num(t.current_max));
                o.set("scale", Json::str(t.scale.as_str()));
                o.set("interface_verified", Json::Bool(t.interface_verified));
                o
            }
            None => Json::Null,
        },
    );
    a.set(
        "global_clamp",
        match uclamp::discover_global_clamp(root) {
            Some((min, max)) => {
                let mut o = Json::obj();
                o.set("min", Json::num(min as f64));
                o.set("max", Json::num(max as f64));
                o
            }
            None => Json::Null,
        },
    );
    a.set("forbidden_rules", Json::num(12.0));
    j.set("actuator", a);

    let mut p = Json::obj();
    p.set(
        "data_dir",
        Json::str(paths.root.to_string_lossy().to_string()),
    );
    p.set("config_exists", Json::Bool(paths.config().exists()));
    p.set("config_path", Json::str(paths.config().to_string_lossy()));
    j.set("paths", p);
    j.set("failures", Json::num(failures as f64));
    j.set(
        "verdict",
        Json::str(if failures == 0 { "ok" } else { "degraded" }),
    );

    (j, failures)
}

fn fmt_list(v: &[u32]) -> String {
    v.iter()
        .map(|x| x.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use neurosched::storage::Paths;

    fn parse(values: &[&str]) -> Result<Opts, String> {
        parse_args(
            &values
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn empty_arguments_select_daemon_with_safe_defaults() {
        let opts = parse(&[]).unwrap();
        assert_eq!(opts.command, Command::Daemon);
        assert!(opts.mode.is_none());
        assert!(opts.port.is_none());
    }

    #[test]
    fn valid_options_and_existing_mode_aliases_are_preserved() {
        let opts = parse(&[
            "--config",
            "custom.json",
            "--data-dir",
            "data",
            "--fixture",
            "device",
            "--mode",
            "dry",
            "--port",
            "65535",
            "--status",
            "--no-api",
        ])
        .unwrap();
        assert_eq!(opts.config, Some(PathBuf::from("custom.json")));
        assert_eq!(opts.data_dir, Some(PathBuf::from("data")));
        assert_eq!(opts.fixture, Some(PathBuf::from("device")));
        assert_eq!(opts.mode, Some(RunMode::DryRun));
        assert_eq!(opts.port, Some(65535));
        assert_eq!(opts.command, Command::Inspect);
        assert!(opts.no_api);
        assert_eq!(
            parse(&["--mode", "enabled"]).unwrap().mode,
            Some(RunMode::Enabled)
        );
        assert_eq!(
            parse(&["--mode", "observe"]).unwrap().mode,
            Some(RunMode::ObserveOnly)
        );
    }

    #[test]
    fn value_options_reject_missing_empty_and_flag_values() {
        for option in ["--config", "--data-dir", "--fixture", "--mode", "--port"] {
            for values in [vec![option], vec![option, ""], vec![option, "--once"]] {
                let error = parse(&values).unwrap_err();
                assert!(error.contains("requires a value"), "{values:?} {error}");
            }
        }
    }

    #[test]
    fn invalid_modes_and_ports_are_rejected() {
        assert!(parse(&["--mode", "boost"]).is_err());
        for value in ["0", "65536", "not-a-port", "1.5", "-1"] {
            assert!(parse(&["--port", value]).is_err(), "{value}");
        }
        assert_eq!(parse(&["--port", "1"]).unwrap().port, Some(1));
        assert!(parse(&["--unknown"]).is_err());
    }

    #[test]
    fn conflicting_commands_are_rejected_in_both_orders() {
        let commands = [
            "--once",
            "--self-test",
            "--print-default-config",
            "--restore-all",
            "--stop",
            "--help",
            "--version",
        ];
        for first in commands {
            for second in commands {
                if first != second {
                    let error = parse(&[first, second]).unwrap_err();
                    assert!(error.contains("conflicting commands"), "{first} {second}");
                }
            }
        }
    }

    #[test]
    fn duplicate_aliases_select_one_operation() {
        assert_eq!(
            parse(&["--once", "--status"]).unwrap().command,
            Command::Inspect
        );
        assert_eq!(parse(&["--stop", "--stop"]).unwrap().command, Command::Stop);
        assert!(parse(&["--help", "--port", "invalid"]).is_err());
    }

    #[test]
    fn missing_thermal_telemetry_is_unknown_and_inspection_creates_no_data() {
        let root_dir =
            std::env::temp_dir().join(format!("neurosched-cli-missing-{}", std::process::id()));
        let data_dir = root_dir.join("data");
        assert!(!data_dir.exists());
        let root = SysRoot::at(&root_dir).read_only();
        let paths = Paths::new(&data_dir).with_config(root_dir.join("custom-config.json"));
        let (report, failures) = self_test_report(&root, &Config::default(), &paths);
        assert!(report.get_path("thermal.level").unwrap().is_null());
        assert!(report.get_path("thermal.cpu_c").unwrap().is_null());
        assert_eq!(
            report.get("ACTUATOR_ENABLED").and_then(Json::as_bool),
            Some(false)
        );
        assert_eq!(
            report
                .get_path("actuator.writes_allowed")
                .and_then(Json::as_bool),
            Some(false)
        );
        assert!(failures > 0);
        assert!(!data_dir.exists());
    }

    #[test]
    fn uclamp_report_contains_raw_percentage_values_and_mount_verification() {
        let root_dir =
            std::env::temp_dir().join(format!("neurosched-cli-uclamp-{}", std::process::id()));
        let cgroup = root_dir.join("sys/fs/cgroup/top-app");
        std::fs::create_dir_all(&cgroup).unwrap();
        std::fs::create_dir_all(root_dir.join("proc/self")).unwrap();
        std::fs::write(cgroup.join("cpu.uclamp.min"), "12.50\n").unwrap();
        std::fs::write(cgroup.join("cpu.uclamp.max"), "max\n").unwrap();
        std::fs::write(
            root_dir.join("proc/self/mountinfo"),
            "29 23 0:25 / /sys/fs/cgroup rw - cgroup2 cgroup rw\n",
        )
        .unwrap();
        let root = SysRoot::at(&root_dir).read_only();
        let paths = Paths::new(root_dir.join("data"));
        let (report, _) = self_test_report(&root, &Config::default(), &paths);
        assert_eq!(
            report
                .get_path("actuator.uclamp.scale")
                .and_then(Json::as_str),
            Some("percent_0_100")
        );
        assert_eq!(
            report
                .get_path("actuator.uclamp.raw_min")
                .and_then(Json::as_str),
            Some("12.50")
        );
        assert_eq!(
            report
                .get_path("actuator.uclamp.raw_max")
                .and_then(Json::as_str),
            Some("max")
        );
        assert_eq!(
            report
                .get_path("actuator.uclamp.interface_verified")
                .and_then(Json::as_bool),
            Some(true)
        );
        assert_eq!(
            report
                .get_path("actuator.write_probe_performed")
                .and_then(Json::as_bool),
            Some(false)
        );
        assert_eq!(
            std::fs::read_to_string(cgroup.join("cpu.uclamp.min")).unwrap(),
            "12.50\n"
        );
        assert_eq!(
            std::fs::read_to_string(cgroup.join("cpu.uclamp.max")).unwrap(),
            "max\n"
        );
        assert!(!paths.root.exists());
    }
}

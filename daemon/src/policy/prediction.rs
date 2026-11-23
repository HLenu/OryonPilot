//! Read-only simulation for a future app launch. This output is deliberately a
//! separate type from `Decision` and has no path to the actuator.

use super::memory_guard::MemoryGuard;
use super::{Decision, MemoryClass, ThermalGuard, ThermalPermission};
use crate::collector::thermal::{freshness, normalize_temp};
use crate::config::{Config, Profile};
use crate::json::Json;
use crate::predictor::Prediction;
use crate::types::{AppEventSource, PolicyState, ScreenState, Snapshot};

#[derive(Debug, Clone)]
pub struct PredictionPreview {
    pub dry_run_only: bool,
    pub writes_allowed: bool,
    pub eligible: bool,
    /// A predicted future application, never an authorization for current top-app.
    pub target_package: Option<String>,
    /// Internal 0..1024 uclamp units, present only for an eligible simulation.
    pub profile: Option<Profile>,
    pub duration_ms: u64,
    pub reason: String,
    pub blocked: Vec<String>,
    pub at_ms: u64,
}

impl Default for PredictionPreview {
    fn default() -> Self {
        Self {
            dry_run_only: true,
            writes_allowed: false,
            eligible: false,
            target_package: None,
            profile: None,
            duration_ms: 0,
            reason: "no prediction preview computed".into(),
            blocked: Vec::new(),
            at_ms: 0,
        }
    }
}

impl PredictionPreview {
    pub fn to_json(&self) -> Json {
        let mut json = Json::obj();
        json.set("dry_run_only", Json::Bool(true));
        json.set("writes_allowed", Json::Bool(false));
        json.set("eligible", Json::Bool(self.eligible));
        json.set("scope", Json::str("future_target_only"));
        json.set(
            "target_package",
            self.target_package
                .clone()
                .map(Json::Str)
                .unwrap_or(Json::Null),
        );
        let profile = self.profile.map(|profile| {
            let mut json = Json::obj();
            json.set("uclamp_min", Json::num(profile.uclamp_min as f64));
            json.set("uclamp_max", Json::num(profile.uclamp_max as f64));
            json.set("unit", Json::str("capacity_1024"));
            json
        });
        json.set("profile", profile.unwrap_or(Json::Null));
        json.set("duration_ms", Json::num(self.duration_ms as f64));
        json.set("reason", Json::str(self.reason.clone()));
        json.set(
            "blocked",
            Json::Arr(self.blocked.iter().cloned().map(Json::Str).collect()),
        );
        json.set("at_ms", Json::num(self.at_ms as f64));
        json
    }
}

/// Simulate the configured launch profile without changing any input or state.
/// Both decision and snapshot must belong to the same sampling tick. Thermal
/// freshness uses the collector's existing budget, including its interval clamp.
pub fn preview(
    snapshot: &Snapshot,
    prediction: &Prediction,
    current_decision: &Decision,
    config: &Config,
) -> PredictionPreview {
    let mut blocked = Vec::new();
    if !config.predictor.enabled {
        blocked.push("predictor disabled".into());
    }
    if prediction.refused {
        blocked.push(format!("prediction refused: {}", prediction.reason));
    }
    let target = prediction
        .package
        .as_deref()
        .filter(|p| !p.trim().is_empty());
    if target.is_none() {
        blocked.push("prediction target unavailable".into());
    }
    if target.is_some_and(|pkg| config.apps.launchers.iter().any(|l| l == pkg)) {
        blocked.push("prediction target is a launcher".into());
    }
    if prediction.sample_count < config.predictor.min_samples {
        blocked.push("prediction has insufficient history".into());
    }
    if !unit_interval(prediction.confidence)
        || !unit_interval(config.predictor.min_confidence)
        || prediction.confidence < config.predictor.min_confidence
    {
        blocked.push("prediction confidence invalid or below configured minimum".into());
    }
    if !unit_interval(prediction.probability) || prediction.probability == 0.0 {
        blocked.push("prediction probability invalid or zero".into());
    }

    let foreground = snapshot
        .apps
        .foreground
        .as_deref()
        .filter(|p| !p.trim().is_empty());
    if foreground.is_none() {
        blocked.push("foreground unavailable".into());
    }
    if !matches!(
        snapshot.apps.source,
        Some(AppEventSource::Logcat | AppEventSource::Companion)
    ) {
        blocked.push("foreground source is not high trust".into());
    }
    if snapshot.apps.since_ms > snapshot.ts_ms {
        blocked.push("foreground timestamp is in the future".into());
    }
    if target.is_some() && target == foreground {
        blocked.push("prediction target is already foreground".into());
    }
    if snapshot.apps.screen != ScreenState::On {
        blocked.push("screen is not confirmed on".into());
    }
    if current_decision.at_ms != snapshot.ts_ms || snapshot.health.last_sample_ms != snapshot.ts_ms
    {
        blocked.push("decision and telemetry do not belong to the same sampling tick".into());
    }
    if protective(current_decision.state)
        || current_decision
            .eligible
            .iter()
            .any(|(state, _)| protective(*state))
    {
        blocked.push("current decision has an active protective state".into());
    }

    let thermal = &snapshot.thermal;
    // Use the same effective interval as ThermalCollector::new/set_policy.
    let thermal_interval = config.sample.thermal_interval_ms.clamp(500, 15_000);
    if !freshness(thermal.trust, thermal.age_ms, thermal_interval).is_trusted()
        || !current_decision.thermal_trust.is_trusted()
    {
        blocked.push("thermal telemetry is untrusted or stale".into());
    }
    if !valid_celsius(thermal.cpu_max_c)
        || (config.policy.thermal.include_skin && !valid_celsius(thermal.skin_c))
    {
        blocked.push("required temperature reading is missing or invalid".into());
    }
    let mut thermal_guard = ThermalGuard::new(config.policy.thermal.clone());
    thermal_guard.update(thermal);
    if thermal_guard.permission() != ThermalPermission::Allow
        || current_decision.permission != ThermalPermission::Allow
    {
        blocked.push("thermal protection suppresses launch boost".into());
    }

    if !valid_memory(snapshot) {
        blocked.push("meminfo or memory PSI is unavailable or invalid".into());
    }
    let mut memory_guard = MemoryGuard::new(config.policy.memory.clone());
    let memory_class = memory_guard.update(&snapshot.mem, &snapshot.psi);
    if memory_guard.is_pressured()
        || memory_class != MemoryClass::Normal
        || current_decision.memory_class != MemoryClass::Normal
    {
        blocked.push("memory protection suppresses launch boost".into());
    }

    let battery = &snapshot.battery;
    let capacity = battery
        .capacity_pct
        .filter(|value| (0.0..=100.0).contains(value));
    if !battery.present || capacity.is_none() {
        blocked.push("battery capacity unavailable or invalid".into());
    }
    if !(config.policy.battery.ignore_while_charging && battery.charging)
        && capacity.is_some_and(|value| value <= config.policy.battery.saver_pct)
    {
        blocked.push("battery is at or below configured saver threshold".into());
    }

    let profile = config.profile_for(PolicyState::AppLaunch);
    if profile.uclamp_min > profile.uclamp_max || profile.uclamp_max > 1024 {
        blocked.push("configured app-launch profile is outside internal uclamp range".into());
    }
    if config.policy.app_launch.boost_ms == 0 {
        blocked.push("configured app-launch duration is zero".into());
    }
    let eligible = blocked.is_empty();
    PredictionPreview {
        eligible,
        target_package: target.map(str::to_owned),
        profile: eligible.then_some(profile),
        duration_ms: if eligible {
            config.policy.app_launch.boost_ms
        } else {
            0
        },
        reason: blocked
            .first()
            .cloned()
            .unwrap_or_else(|| "future app-launch profile preview only".into()),
        blocked,
        at_ms: snapshot.ts_ms,
        ..Default::default()
    }
}

fn protective(state: PolicyState) -> bool {
    matches!(
        state,
        PolicyState::Boot
            | PolicyState::SafeMode
            | PolicyState::ThermalGuard
            | PolicyState::MemoryPressure
            | PolicyState::BatterySaver
    )
}

fn unit_interval(value: f64) -> bool {
    (0.0..=1.0).contains(&value)
}

fn valid_celsius(value: Option<f64>) -> bool {
    value.is_some_and(|value| normalize_temp(value) == Some(value))
}

fn valid_memory(snapshot: &Snapshot) -> bool {
    let mem = &snapshot.mem;
    let psi = &snapshot.psi.memory;
    mem.meminfo_ok
        && mem.total_kb.is_finite()
        && mem.total_kb > 0.0
        && (0.0..=mem.total_kb).contains(&mem.available_kb)
        && [mem.kswapd_cpu_pct, mem.swap_in_rate_kb_s, mem.pswpin]
            .iter()
            .all(|value| value.is_finite() && *value >= 0.0)
        && psi.available
        && [
            psi.some10,
            psi.some60,
            psi.some300,
            psi.full10,
            psi.full60,
            psi.full300,
        ]
        .iter()
        .all(|value| (0.0..=100.0).contains(value))
        && [psi.some_total, psi.full_total]
            .iter()
            .all(|value| value.is_finite() && *value >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AppSnapshot, BatterySnapshot, Health, MemSnapshot, PsiRes, PsiSnapshot, ThermalSnapshot,
        ThermalTrust,
    };

    fn fixture() -> (Snapshot, Prediction, Decision, Config) {
        let config = Config::default();
        let snapshot = Snapshot {
            ts_ms: 100_000,
            apps: AppSnapshot {
                foreground: Some("com.current.app".into()),
                source: Some(AppEventSource::Logcat),
                screen: ScreenState::On,
                since_ms: 90_000,
            },
            thermal: ThermalSnapshot {
                trust: ThermalTrust::Trusted,
                cpu_max_c: Some(40.0),
                ..Default::default()
            },
            mem: MemSnapshot {
                meminfo_ok: true,
                total_kb: 10_000.0,
                available_kb: 5_000.0,
                ..Default::default()
            },
            psi: PsiSnapshot {
                memory: PsiRes {
                    available: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            battery: BatterySnapshot {
                present: true,
                capacity_pct: Some(80.0),
                ..Default::default()
            },
            health: Health {
                last_sample_ms: 100_000,
                ..Default::default()
            },
            ..Default::default()
        };
        let prediction = Prediction {
            package: Some("com.future.app".into()),
            probability: 0.8,
            confidence: 0.7,
            sample_count: config.predictor.min_samples,
            ..Default::default()
        };
        let decision = Decision {
            state: PolicyState::Foreground,
            thermal_trust: ThermalTrust::Trusted,
            at_ms: snapshot.ts_ms,
            ..Default::default()
        };
        (snapshot, prediction, decision, config)
    }

    fn assert_blocked(preview: PredictionPreview, expected: &str) {
        assert!(!preview.eligible);
        assert!(preview.dry_run_only);
        assert!(!preview.writes_allowed);
        assert!(preview.profile.is_none());
        assert_eq!(preview.duration_ms, 0);
        assert!(
            preview
                .blocked
                .iter()
                .any(|reason| reason.contains(expected)),
            "missing {expected:?} in {:?}",
            preview.blocked
        );
    }

    #[test]
    fn eligible_preview_reuses_config_and_preserves_current_decision() {
        let (snapshot, prediction, decision, mut config) = fixture();
        config.policy.app_launch.boost_ms = 1_234;
        let before = decision.to_json();
        let result = preview(&snapshot, &prediction, &decision, &config);
        assert!(result.eligible);
        assert!(result.dry_run_only);
        assert!(!result.writes_allowed);
        assert_eq!(result.target_package.as_deref(), Some("com.future.app"));
        assert_eq!(
            result.profile,
            Some(config.profile_for(PolicyState::AppLaunch))
        );
        assert_eq!(result.duration_ms, 1_234);
        assert_eq!(decision.to_json(), before);
        let json = result.to_json();
        assert_eq!(json.get("dry_run_only").and_then(Json::as_bool), Some(true));
        assert_eq!(
            json.get("writes_allowed").and_then(Json::as_bool),
            Some(false)
        );
        assert_eq!(
            json.get("scope").and_then(Json::as_str),
            Some("future_target_only")
        );
    }

    #[test]
    fn execution_mode_and_consent_never_allow_preview_writes() {
        let (snapshot, prediction, decision, mut config) = fixture();
        config.actuator.enabled = true;
        for mode in [
            crate::types::RunMode::ObserveOnly,
            crate::types::RunMode::DryRun,
            crate::types::RunMode::Enabled,
        ] {
            config.mode = mode;
            let result = preview(&snapshot, &prediction, &decision, &config);
            assert!(result.eligible);
            assert!(result.dry_run_only);
            assert!(!result.writes_allowed);
        }
    }

    #[test]
    fn foreground_requires_high_trust_and_known_screen() {
        let (mut snapshot, prediction, decision, config) = fixture();
        for source in [
            None,
            Some(AppEventSource::None),
            Some(AppEventSource::ProcScan),
        ] {
            snapshot.apps.source = source;
            assert_blocked(
                preview(&snapshot, &prediction, &decision, &config),
                "high trust",
            );
        }
        snapshot.apps.source = Some(AppEventSource::Companion);
        assert!(preview(&snapshot, &prediction, &decision, &config).eligible);
        for screen in [ScreenState::Unknown, ScreenState::Off] {
            snapshot.apps.screen = screen;
            assert_blocked(
                preview(&snapshot, &prediction, &decision, &config),
                "screen",
            );
        }
        snapshot.apps.screen = ScreenState::On;
        snapshot.apps.foreground = Some("  ".into());
        assert_blocked(
            preview(&snapshot, &prediction, &decision, &config),
            "foreground unavailable",
        );
    }

    #[test]
    fn timestamps_must_agree_and_foreground_cannot_come_from_future() {
        let (mut snapshot, prediction, mut decision, config) = fixture();
        decision.at_ms -= 1;
        assert_blocked(
            preview(&snapshot, &prediction, &decision, &config),
            "same sampling tick",
        );
        decision.at_ms = snapshot.ts_ms;
        snapshot.health.last_sample_ms -= 1;
        assert_blocked(
            preview(&snapshot, &prediction, &decision, &config),
            "same sampling tick",
        );
        snapshot.health.last_sample_ms = snapshot.ts_ms;
        snapshot.apps.since_ms = snapshot.ts_ms + 1;
        assert_blocked(
            preview(&snapshot, &prediction, &decision, &config),
            "future",
        );
    }

    #[test]
    fn every_protective_state_vetoes_preview_even_before_transition() {
        let (snapshot, prediction, mut decision, config) = fixture();
        for state in [
            PolicyState::Boot,
            PolicyState::SafeMode,
            PolicyState::ThermalGuard,
            PolicyState::MemoryPressure,
            PolicyState::BatterySaver,
        ] {
            decision.state = state;
            assert_blocked(
                preview(&snapshot, &prediction, &decision, &config),
                "protective state",
            );
            decision.state = PolicyState::Foreground;
            decision.eligible = vec![(state, "pending transition".into())];
            assert_blocked(
                preview(&snapshot, &prediction, &decision, &config),
                "protective state",
            );
            decision.eligible.clear();
        }
    }

    #[test]
    fn thermal_trust_missing_invalid_and_stale_readings_veto_preview() {
        let (mut snapshot, prediction, decision, config) = fixture();
        for trust in [
            ThermalTrust::Unknown,
            ThermalTrust::NoSensor,
            ThermalTrust::Invalid,
            ThermalTrust::Stale,
            ThermalTrust::ServiceUnavailable,
            ThermalTrust::Ambiguous,
        ] {
            snapshot.thermal.trust = trust;
            assert_blocked(
                preview(&snapshot, &prediction, &decision, &config),
                "thermal telemetry",
            );
        }
        snapshot.thermal.trust = ThermalTrust::Trusted;
        for reading in [
            None,
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(-273.0),
            Some(40_000.0),
        ] {
            snapshot.thermal.cpu_max_c = reading;
            assert_blocked(
                preview(&snapshot, &prediction, &decision, &config),
                "temperature reading",
            );
        }
        snapshot.thermal.cpu_max_c = Some(40.0);
        snapshot.thermal.age_ms = u64::MAX;
        assert_blocked(preview(&snapshot, &prediction, &decision, &config), "stale");
    }

    #[test]
    fn thermal_policy_and_hysteresis_permission_are_respected() {
        let (mut snapshot, prediction, mut decision, mut config) = fixture();
        snapshot.thermal.cpu_max_c = Some(config.policy.thermal.enter_light_c);
        assert_blocked(
            preview(&snapshot, &prediction, &decision, &config),
            "thermal protection",
        );
        snapshot.thermal.cpu_max_c = Some(40.0);
        for permission in [
            ThermalPermission::Trim,
            ThermalPermission::NoBoost,
            ThermalPermission::Cap,
        ] {
            decision.permission = permission;
            assert_blocked(
                preview(&snapshot, &prediction, &decision, &config),
                "thermal protection",
            );
        }
        decision.permission = ThermalPermission::Allow;
        config.policy.thermal.include_skin = true;
        assert_blocked(
            preview(&snapshot, &prediction, &decision, &config),
            "temperature reading",
        );
        snapshot.thermal.skin_c = Some(config.policy.thermal.enter_moderate_c);
        assert_blocked(
            preview(&snapshot, &prediction, &decision, &config),
            "thermal protection",
        );
    }

    #[test]
    fn missing_or_invalid_memory_telemetry_vetoes_preview() {
        let (snapshot, prediction, decision, config) = fixture();
        for variant in 0..7 {
            let mut changed = snapshot.clone();
            match variant {
                0 => changed.mem.meminfo_ok = false,
                1 => changed.mem.total_kb = f64::INFINITY,
                2 => changed.mem.available_kb = changed.mem.total_kb + 1.0,
                3 => changed.psi.memory.available = false,
                4 => changed.psi.memory.some10 = f64::NAN,
                5 => changed.psi.memory.full60 = 101.0,
                _ => changed.mem.swap_in_rate_kb_s = -1.0,
            }
            assert_blocked(
                preview(&changed, &prediction, &decision, &config),
                "meminfo or memory PSI",
            );
        }
    }

    #[test]
    fn memory_pressure_uses_current_config_and_retains_engine_protection() {
        let (mut snapshot, prediction, mut decision, config) = fixture();
        snapshot.psi.memory.some10 = config.policy.memory.psi_some_enter + 1.0;
        assert_blocked(
            preview(&snapshot, &prediction, &decision, &config),
            "memory protection",
        );
        snapshot.psi.memory.some10 = 0.0;
        snapshot.mem.available_kb =
            snapshot.mem.total_kb * config.policy.memory.avail_pct_enter / 200.0;
        assert_blocked(
            preview(&snapshot, &prediction, &decision, &config),
            "memory protection",
        );
        snapshot.mem.available_kb = snapshot.mem.total_kb / 2.0;
        decision.memory_class = MemoryClass::Reclaiming;
        assert_blocked(
            preview(&snapshot, &prediction, &decision, &config),
            "memory protection",
        );
    }

    #[test]
    fn battery_uses_existing_charging_exception_and_rejects_unknown_capacity() {
        let (mut snapshot, prediction, decision, mut config) = fixture();
        snapshot.battery.capacity_pct = Some(config.policy.battery.saver_pct);
        assert_blocked(
            preview(&snapshot, &prediction, &decision, &config),
            "saver threshold",
        );
        snapshot.battery.charging = true;
        assert!(preview(&snapshot, &prediction, &decision, &config).eligible);
        config.policy.battery.ignore_while_charging = false;
        assert_blocked(
            preview(&snapshot, &prediction, &decision, &config),
            "saver threshold",
        );
        for capacity in [None, Some(f64::NAN), Some(-1.0), Some(101.0)] {
            snapshot.battery.capacity_pct = capacity;
            assert_blocked(
                preview(&snapshot, &prediction, &decision, &config),
                "battery capacity",
            );
        }
    }

    #[test]
    fn disabled_refused_empty_or_current_predictions_are_rejected() {
        let (snapshot, mut prediction, decision, mut config) = fixture();
        config.predictor.enabled = false;
        assert_blocked(
            preview(&snapshot, &prediction, &decision, &config),
            "disabled",
        );
        config.predictor.enabled = true;
        prediction.refused = true;
        assert_blocked(
            preview(&snapshot, &prediction, &decision, &config),
            "refused",
        );
        prediction.refused = false;
        for package in [None, Some(" ".into())] {
            prediction.package = package;
            assert_blocked(
                preview(&snapshot, &prediction, &decision, &config),
                "target unavailable",
            );
        }
        prediction.package = snapshot.apps.foreground.clone();
        assert_blocked(
            preview(&snapshot, &prediction, &decision, &config),
            "already foreground",
        );
        prediction.package = Some(config.apps.launchers[0].clone());
        assert_blocked(
            preview(&snapshot, &prediction, &decision, &config),
            "launcher",
        );
    }

    #[test]
    fn insufficient_history_and_invalid_scores_are_rejected() {
        let (snapshot, prediction, decision, config) = fixture();
        let mut changed = prediction.clone();
        changed.sample_count = config.predictor.min_samples - 1;
        assert_blocked(
            preview(&snapshot, &changed, &decision, &config),
            "insufficient history",
        );
        for confidence in [
            f64::NAN,
            f64::INFINITY,
            -0.1,
            1.1,
            config.predictor.min_confidence / 2.0,
        ] {
            changed = prediction.clone();
            changed.confidence = confidence;
            assert_blocked(
                preview(&snapshot, &changed, &decision, &config),
                "confidence",
            );
        }
        for probability in [f64::NAN, f64::NEG_INFINITY, -0.1, 0.0, 1.1] {
            changed = prediction.clone();
            changed.probability = probability;
            assert_blocked(
                preview(&snapshot, &changed, &decision, &config),
                "probability",
            );
        }
    }

    #[test]
    fn invalid_profile_and_zero_duration_are_rejected() {
        let (snapshot, prediction, decision, mut config) = fixture();
        let profile = config
            .actuator
            .profiles
            .iter_mut()
            .find(|(state, _)| *state == PolicyState::AppLaunch)
            .unwrap();
        profile.1.uclamp_min = 1025;
        assert_blocked(
            preview(&snapshot, &prediction, &decision, &config),
            "uclamp range",
        );
        config = Config::default();
        config.policy.app_launch.boost_ms = 0;
        assert_blocked(
            preview(&snapshot, &prediction, &decision, &config),
            "duration is zero",
        );
    }
}

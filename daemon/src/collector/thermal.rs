//! Thermal zone discovery, classification and **trust**.
//!
//! Three rules are enforced here:
//!  1. A zone is classified by its `type` string, never by its index.
//!  2. "CPU temperature" is the max over CPU-class zones only. Taking the max over
//!     every zone is how modules end up throttling because the charger got warm.
//!  3. If the sensor cannot be trusted, `cpu_max_c` becomes `None` and
//!     [`ThermalTrust`] says exactly why. The policy layer treats that as
//!     fail-closed: no boost, no aggressive cap, and the daemon drops to
//!     `observe_only`.

use crate::config::ThermalPolicy;
use crate::sys::SysRoot;
use crate::types::{ThermalLevel, ThermalSnapshot, ThermalTrust, ThermalZoneInfo, ZoneKind};

const THERMAL: &str = "/sys/class/thermal";
/// A reading older than this is never used for a decision, whatever the cache says.
const MAX_AGE_MS: u64 = 15_000;

pub struct ThermalCollector {
    policy: ThermalPolicy,
    cached: Option<ThermalSnapshot>,
    last_read_ms: u64,
    last_ok_ms: Option<u64>,
    interval_ms: u64,
}

impl ThermalCollector {
    pub fn new(policy: ThermalPolicy, interval_ms: u64) -> Self {
        Self {
            policy,
            cached: None,
            last_read_ms: 0,
            last_ok_ms: None,
            interval_ms: interval_ms.clamp(500, MAX_AGE_MS),
        }
    }

    pub fn set_policy(&mut self, policy: ThermalPolicy, interval_ms: u64) {
        self.policy = policy;
        self.interval_ms = interval_ms.clamp(500, MAX_AGE_MS);
    }

    /// Thermal data changes slowly; re-reading it at 1 Hz would be pure waste.
    pub fn collect(&mut self, root: &SysRoot, now: u64) -> ThermalSnapshot {
        let due = self.cached.is_none()
            || now.saturating_sub(self.last_read_ms) >= self.interval_ms
            || self.last_ok_ms.is_none();
        if due {
            let snap = self.read(root, now);
            if snap.trust.is_trusted() {
                self.last_ok_ms = Some(now);
            }
            self.cached = Some(snap);
            self.last_read_ms = now;
        }

        let mut snap = match &self.cached {
            Some(s) => s.clone(),
            None => return self.unknown(now, ThermalTrust::Unknown),
        };

        // Freshness is evaluated on every call, even when the cached sample is reused:
        // a sensor that stopped updating must not keep a stale value alive forever.
        let age = self
            .last_ok_ms
            .map(|last| now.saturating_sub(last))
            .unwrap_or(u64::MAX);
        snap.age_ms = age;

        if freshness(snap.trust, age, self.interval_ms) == ThermalTrust::Stale {
            let mut stale = self.unknown(now, ThermalTrust::Stale);
            stale.detail = format!(
                "{} (last valid sample {} ms ago)",
                ThermalTrust::Stale.reason(),
                age.min(86_400_000)
            );
            stale.age_ms = age;
            snap = stale;
        }
        snap
    }

    fn unknown(&self, now: u64, trust: ThermalTrust) -> ThermalSnapshot {
        ThermalSnapshot {
            trust,
            detail: trust.reason().to_string(),
            age_ms: self
                .last_ok_ms
                .map(|last| now.saturating_sub(last))
                .unwrap_or(u64::MAX),
            level: None,
            level_source: "untrusted".into(),
            ..Default::default()
        }
    }

    fn read(&mut self, root: &SysRoot, now: u64) -> ThermalSnapshot {
        let dir_present = root.exists(THERMAL);
        let names: Vec<String> = root
            .list(THERMAL)
            .into_iter()
            .filter(|n| n.starts_with("thermal_zone"))
            .collect();

        if names.is_empty() {
            let trust = if dir_present {
                ThermalTrust::NoSensor
            } else {
                ThermalTrust::ServiceUnavailable
            };
            return self.unknown(now, trust);
        }

        let mut zones = Vec::new();
        let mut invalid_zones = 0usize;
        let mut unreadable_zones = 0usize;

        for name in &names {
            let base = format!("{THERMAL}/{name}");
            let Some(kind_str) = root.read_trim(&format!("{base}/type")) else {
                unreadable_zones += 1;
                continue;
            };
            // Qualcomm exposes battery-limit virtual sensors in the thermal
            // tree too. Their units are voltage/current/percentage or a level,
            // not Celsius. In particular, `socd` is battery state of charge,
            // so the general `soc` CPU-name heuristic must never see it.
            let name_lower = kind_str.to_ascii_lowercase();
            if matches!(name_lower.as_str(), "vbat" | "ibat" | "socd")
                || name_lower.contains("-bcl-")
                || name_lower.starts_with("bcl-")
            {
                continue;
            }
            let Some(raw) = root.read_trim(&format!("{base}/temp")) else {
                unreadable_zones += 1;
                continue;
            };
            let Ok(v) = raw.parse::<f64>() else {
                invalid_zones += 1;
                continue;
            };
            match normalize_temp(v) {
                Some(temp_c) => zones.push(ThermalZoneInfo {
                    kind: ZoneKind::classify(&kind_str),
                    name: kind_str,
                    temp_c,
                }),
                None => invalid_zones += 1,
            }
        }

        let max_of = |kind: ZoneKind| -> Option<f64> {
            zones
                .iter()
                .filter(|z| z.kind == kind)
                .map(|z| z.temp_c)
                .fold(None, |acc: Option<f64>, t| {
                    Some(acc.map_or(t, |a| a.max(t)))
                })
        };

        let cpu_max = max_of(ZoneKind::Cpu);
        let has_unknown_kind = zones.iter().any(|z| z.kind == ZoneKind::Other);
        let has_any = !zones.is_empty();

        let (trust, detail) = if cpu_max.is_some() {
            (ThermalTrust::Trusted, String::new())
        } else if !has_any {
            // Every zone failed to parse or was out of range.
            (
                ThermalTrust::Invalid,
                format!(
                    "{} ({} unparsable, {} unreadable, {} total)",
                    ThermalTrust::Invalid.reason(),
                    invalid_zones,
                    unreadable_zones,
                    names.len()
                ),
            )
        } else if has_unknown_kind {
            // Zones we cannot classify might be the CPU sensor; guessing would be worse
            // than refusing to boost.
            (
                ThermalTrust::Ambiguous,
                format!(
                    "{} ({} zone(s), none matched a CPU keyword)",
                    ThermalTrust::Ambiguous.reason(),
                    zones.len()
                ),
            )
        } else {
            (
                ThermalTrust::NoSensor,
                format!(
                    "{} ({} zone(s) mapped, kinds: {})",
                    ThermalTrust::NoSensor.reason(),
                    zones.len(),
                    zones
                        .iter()
                        .map(|z| z.kind.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            )
        };

        let mut snap = ThermalSnapshot {
            cpu_max_c: if trust.is_trusted() { cpu_max } else { None },
            cpu_c: if trust.is_trusted() { cpu_max } else { None },
            gpu_c: max_of(ZoneKind::Gpu),
            skin_c: max_of(ZoneKind::Skin),
            battery_c: max_of(ZoneKind::Battery),
            level: None,
            level_source: if trust.is_trusted() {
                "derived".into()
            } else {
                "untrusted".into()
            },
            zones,
            trust,
            detail,
            age_ms: 0,
            invalid_zones,
        };
        snap.level = if trust.is_trusted() {
            Some(self.level_for(&snap))
        } else {
            None
        };
        snap
    }

    /// Composite level. Skin temperature is included only when explicitly enabled:
    /// on a phone held in the hand, skin sensors are the user-visible constraint,
    /// but they also run much hotter than the SoC during charging.
    pub fn level_for(&self, snap: &ThermalSnapshot) -> ThermalLevel {
        let mut hottest = snap.cpu_max_c.unwrap_or(f64::MIN);
        if self.policy.include_skin {
            if let Some(s) = snap.skin_c {
                hottest = hottest.max(s);
            }
        }
        if hottest == f64::MIN {
            return ThermalLevel::Normal;
        }
        if hottest >= self.policy.enter_critical_c {
            ThermalLevel::Critical
        } else if hottest >= self.policy.enter_severe_c {
            ThermalLevel::Severe
        } else if hottest >= self.policy.enter_moderate_c {
            ThermalLevel::Moderate
        } else if hottest >= self.policy.enter_light_c {
            ThermalLevel::Light
        } else {
            ThermalLevel::Normal
        }
    }
}

/// Freshness rule, separated so it can be tested without a clock.
///
/// The collector re-reads at most every `interval_ms`, so in normal operation `age`
/// never exceeds the interval and this stays `Trusted`. It becomes `Stale` only when a
/// sample is being served long after it was taken (stalled thread, clock jump) — which
/// is exactly the case where acting on it would be unsafe.
pub fn freshness(trust: ThermalTrust, age_ms: u64, interval_ms: u64) -> ThermalTrust {
    if !trust.is_trusted() {
        return trust;
    }
    let budget = (interval_ms.saturating_mul(3)).max(MAX_AGE_MS);
    if age_ms == u64::MAX || age_ms > budget {
        ThermalTrust::Stale
    } else {
        ThermalTrust::Trusted
    }
}

/// Thermal zones report millidegrees Celsius on every Android kernel in use, but a
/// few vendor drivers report degrees. Values outside a plausible range are dropped
/// rather than clamped into a fake reading.
pub fn normalize_temp(raw: f64) -> Option<f64> {
    let c = if raw.abs() > 1_000.0 {
        raw / 1000.0
    } else {
        raw
    };
    if (-40.0..=150.0).contains(&c) {
        Some(c)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn fixture(name: &str, zones: &[(&str, &str)]) -> SysRoot {
        let dir = std::env::temp_dir().join(format!("neurosched-thermal-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sys/class/thermal")).unwrap();
        for (i, (ztype, temp)) in zones.iter().enumerate() {
            let p = dir.join(format!("sys/class/thermal/thermal_zone{i}"));
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("type"), ztype).unwrap();
            std::fs::write(p.join("temp"), temp).unwrap();
        }
        SysRoot::at(&dir)
    }

    #[test]
    fn battery_limit_virtual_sensors_are_not_temperatures() {
        // Xiaomi 15 thermalservice reports vbat/ibat/socd with HAL types 6/7/8
        // (BCL_VOLTAGE/CURRENT/PERCENTAGE), not CPU type 0.
        let nonthermal = [
            ("socd", "98"),
            ("vbat", "4121"),
            ("ibat", "200000"),
            ("pm8550-bcl-lvl0", "0"),
        ];
        let mut zones = nonthermal.to_vec();
        zones.push(("cpu-0-0", "40000"));
        let root = fixture("bcl-with-cpu", &zones);
        let mut c = ThermalCollector::new(Config::default().policy.thermal, 1_000);
        let snap = c.collect(&root, 1_000);
        assert_eq!(snap.cpu_c, Some(40.0));
        assert_eq!(snap.zones.len(), 1);
        let root = fixture("bcl-without-cpu", &nonthermal);
        let snap = c.collect(&root, 2_000);
        assert!(!snap.trust.is_trusted());
        assert!(snap.cpu_max_c.is_none());
        assert!(snap.zones.is_empty());
    }

    #[test]
    fn sample_at_monotonic_zero_is_valid_and_cached() {
        let root = fixture("zero-time", &[("cpu-0-0", "45000")]);
        let mut c = ThermalCollector::new(Config::default().policy.thermal, 4_000);
        let first = c.collect(&root, 0);
        assert_eq!(first.trust, ThermalTrust::Trusted);
        assert_eq!(first.cpu_c, Some(45.0));
        assert_eq!(first.age_ms, 0);
        std::fs::write(
            root.resolve("/sys/class/thermal/thermal_zone0/temp"),
            "47000",
        )
        .unwrap();
        let cached = c.collect(&root, 1_000);
        assert_eq!(cached.cpu_c, Some(45.0));
        assert_eq!(cached.age_ms, 1_000);
        assert_eq!(c.collect(&root, 4_000).cpu_c, Some(47.0));
    }

    #[test]
    fn cpu_temperature_ignores_hot_charger_zone() {
        let root = fixture(
            "charger",
            &[
                ("cpu-0-0", "76000"),
                ("gpuss-0", "48000"),
                ("charger", "88000"),
                ("battery", "35000"),
            ],
        );
        let mut c = ThermalCollector::new(Config::default().policy.thermal, 0);
        let snap = c.collect(&root, 1_000);
        assert_eq!(snap.trust, ThermalTrust::Trusted);
        assert_eq!(snap.cpu_c, Some(76.0));
        assert_eq!(snap.gpu_c, Some(48.0));
        assert_eq!(snap.battery_c, Some(35.0));
        assert_eq!(snap.level, Some(ThermalLevel::Moderate));
    }

    #[test]
    fn degrees_and_millidegrees_both_work() {
        assert_eq!(normalize_temp(45_000.0), Some(45.0));
        assert_eq!(normalize_temp(45.0), Some(45.0));
        assert_eq!(normalize_temp(-273_000.0), None);
        assert_eq!(normalize_temp(999_999.0), None);
    }

    #[test]
    fn missing_zones_fail_closed() {
        let root = SysRoot::at(std::env::temp_dir().join("neurosched-thermal-empty"));
        let mut c = ThermalCollector::new(Config::default().policy.thermal, 0);
        let snap = c.collect(&root, 1_000);
        assert!(snap.cpu_c.is_none());
        assert_eq!(snap.trust, ThermalTrust::ServiceUnavailable);
        assert!(snap.level.is_none());
    }

    #[test]
    fn unmappable_zones_are_ambiguous_not_guessed() {
        let root = fixture(
            "ambiguous",
            &[("pm8350_tz", "41000"), ("xo_therm", "39000")],
        );
        let mut c = ThermalCollector::new(Config::default().policy.thermal, 0);
        let snap = c.collect(&root, 1_000);
        assert_eq!(snap.trust, ThermalTrust::Ambiguous);
        assert!(snap.cpu_max_c.is_none());
        assert!(snap.detail.contains("none matched"));
    }

    #[test]
    fn only_non_cpu_zones_is_no_sensor() {
        let root = fixture("nonsensor", &[("gpuss-0", "48000"), ("battery", "35000")]);
        let mut c = ThermalCollector::new(Config::default().policy.thermal, 0);
        let snap = c.collect(&root, 1_000);
        assert_eq!(snap.trust, ThermalTrust::NoSensor);
    }

    #[test]
    fn invalid_values_are_reported() {
        let root = fixture("invalid", &[("cpu-0-0", "999999"), ("cpu-1-0", "-500000")]);
        let mut c = ThermalCollector::new(Config::default().policy.thermal, 0);
        let snap = c.collect(&root, 1_000);
        assert_eq!(snap.trust, ThermalTrust::Invalid);
        assert_eq!(snap.invalid_zones, 2);
        assert!(snap.cpu_max_c.is_none());
    }

    #[test]
    fn disappearing_sensor_is_invalid_not_trusted() {
        let root = fixture("vanish", &[("cpu-0-0", "45000")]);
        let mut c = ThermalCollector::new(Config::default().policy.thermal, 1_000);
        assert_eq!(c.collect(&root, 1_000).trust, ThermalTrust::Trusted);
        std::fs::remove_file(root.resolve("/sys/class/thermal/thermal_zone0/temp")).unwrap();
        let snap = c.collect(&root, 60_000);
        assert!(!snap.trust.is_trusted());
        assert!(snap.cpu_max_c.is_none());
    }

    #[test]
    fn staleness_rule_is_bounded_by_the_interval() {
        // Normal operation: sample age <= interval -> trusted.
        assert_eq!(
            freshness(ThermalTrust::Trusted, 3_000, 4_000),
            ThermalTrust::Trusted
        );
        // A stalled collector must not keep acting on an old reading.
        assert_eq!(
            freshness(ThermalTrust::Trusted, 60_000, 1_000),
            ThermalTrust::Stale
        );
        // An untrusted state is never upgraded by freshness.
        assert_eq!(
            freshness(ThermalTrust::NoSensor, 0, 1_000),
            ThermalTrust::NoSensor
        );
    }

    #[test]
    fn interval_is_clamped_to_the_freshness_budget() {
        let root = fixture("clamp", &[("cpu-0-0", "40000")]);
        let mut c = ThermalCollector::new(Config::default().policy.thermal, 600_000);
        assert!(c.interval_ms <= MAX_AGE_MS);
        assert_eq!(c.collect(&root, 1_000).trust, ThermalTrust::Trusted);
    }

    #[test]
    fn cache_respects_interval() {
        let root = fixture("cache", &[("cpu-0-0", "40000")]);
        let mut c = ThermalCollector::new(Config::default().policy.thermal, 4_000);
        let a = c.collect(&root, 1_000);
        let b = c.collect(&root, 2_000);
        assert_eq!(a.cpu_c, b.cpu_c);
    }
}

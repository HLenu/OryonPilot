//! Thermal guard: decides whether the policy is allowed to raise anything, and how
//! much headroom is left.
//!
//! This module never writes thermal nodes. It only converts sensor readings into a
//! permission level. The Safety layer can veto the predictor; the predictor can never
//! veto this.

use crate::config::ThermalPolicy;
use crate::types::{ThermalLevel, ThermalSnapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThermalPermission {
    /// Full range available.
    Allow,
    /// Mild reduction; boosts still allowed but shorter.
    Trim,
    /// No boost of any kind. Ceiling capped.
    NoBoost,
    /// Actively cap; the only goal is to shed heat.
    Cap,
}

impl ThermalPermission {
    pub fn as_str(self) -> &'static str {
        match self {
            ThermalPermission::Allow => "allow",
            ThermalPermission::Trim => "trim",
            ThermalPermission::NoBoost => "no_boost",
            ThermalPermission::Cap => "cap",
        }
    }
}

pub struct ThermalGuard {
    policy: ThermalPolicy,
    level: ThermalLevel,
    hot: bool,
}

impl ThermalGuard {
    pub fn new(policy: ThermalPolicy) -> Self {
        Self {
            policy,
            level: ThermalLevel::Normal,
            hot: false,
        }
    }

    /// Hysteresis: enter guard at `enter_moderate_c`, leave only below
    /// `enter_moderate_c - exit_margin_c`, so a device hovering near that boundary
    /// does not oscillate.
    pub fn update(&mut self, snap: &ThermalSnapshot) -> ThermalLevel {
        let measured = self.classify(snap);
        let hottest = self.effective_temp(snap).unwrap_or(0.0);
        let exit_at = (self.policy.enter_moderate_c - self.policy.exit_margin_c).max(20.0);

        if measured >= ThermalLevel::Moderate {
            self.hot = true;
        } else if self.hot && hottest <= exit_at {
            self.hot = false;
        }
        self.level = if self.hot {
            measured.max(ThermalLevel::Moderate)
        } else {
            measured
        };
        self.level
    }

    fn effective_temp(&self, snap: &ThermalSnapshot) -> Option<f64> {
        let mut hottest = snap.cpu_max_c;
        if self.policy.include_skin {
            if let Some(skin) = snap.skin_c {
                hottest = Some(hottest.map_or(skin, |cpu| cpu.max(skin)));
            }
        }
        hottest
    }

    /// Derives the level from measured temperatures and *this* policy, so a config
    /// reload cannot leave the guard using stale thresholds.
    fn classify(&self, snap: &ThermalSnapshot) -> ThermalLevel {
        let Some(hottest) = self.effective_temp(snap) else {
            // No CPU-class sensor at all: fall back to whatever the collector derived.
            return snap.level.unwrap_or(ThermalLevel::Normal);
        };
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

    pub fn level(&self) -> ThermalLevel {
        self.level
    }

    pub fn is_guarding(&self) -> bool {
        self.level >= ThermalLevel::Moderate
    }

    pub fn permission(&self) -> ThermalPermission {
        match self.level {
            ThermalLevel::Normal => ThermalPermission::Allow,
            ThermalLevel::Light => ThermalPermission::Trim,
            ThermalLevel::Moderate => ThermalPermission::NoBoost,
            ThermalLevel::Severe | ThermalLevel::Critical | ThermalLevel::Emergency => {
                ThermalPermission::Cap
            }
        }
    }

    /// Hard ceiling for `uclamp.max` while this permission level is active.
    pub fn ceiling(&self) -> u32 {
        match self.permission() {
            ThermalPermission::Allow => 1024,
            ThermalPermission::Trim => 960,
            ThermalPermission::NoBoost => 800,
            ThermalPermission::Cap => 640,
        }
    }

    pub fn describe(&self, snap: &ThermalSnapshot) -> String {
        format!(
            "thermal={} cpu={} gpu={} skin={} perm={}",
            self.level.as_str(),
            fmt(snap.cpu_max_c),
            fmt(snap.gpu_c),
            fmt(snap.skin_c),
            self.permission().as_str()
        )
    }
}

fn fmt(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.1}C"))
        .unwrap_or_else(|| "n/a".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn snap(cpu: Option<f64>, level: ThermalLevel) -> ThermalSnapshot {
        ThermalSnapshot {
            cpu_max_c: cpu,
            cpu_c: cpu,
            level: Some(level),
            ..Default::default()
        }
    }

    #[test]
    fn moderate_engages_guard_and_caps() {
        let mut g = ThermalGuard::new(Config::default().policy.thermal);
        let l = g.update(&snap(Some(76.0), ThermalLevel::Moderate));
        assert_eq!(l, ThermalLevel::Moderate);
        assert!(g.is_guarding());
        assert_eq!(g.permission(), ThermalPermission::NoBoost);
        assert!(g.ceiling() < 1024);
    }

    #[test]
    fn hysteresis_prevents_flapping() {
        let mut g = ThermalGuard::new(Config::default().policy.thermal);
        g.update(&snap(Some(76.0), ThermalLevel::Moderate));
        // 71 C is below moderate (75) but above exit (70) -> still guarding
        assert_eq!(
            g.update(&snap(Some(71.0), ThermalLevel::Light)),
            ThermalLevel::Moderate
        );
        // 70 C is at exit threshold -> released back to measured level (Light)
        assert_eq!(
            g.update(&snap(Some(70.0), ThermalLevel::Light)),
            ThermalLevel::Light
        );
        // 64 C is below light (65) -> Normal
        assert_eq!(
            g.update(&snap(Some(64.0), ThermalLevel::Normal)),
            ThermalLevel::Normal
        );
    }

    #[test]
    fn include_skin_applies_to_hysteresis_exit() {
        let mut policy = Config::default().policy.thermal;
        policy.include_skin = true;
        let mut g = ThermalGuard::new(policy);
        let mut s = snap(Some(60.0), ThermalLevel::Normal);
        s.skin_c = Some(76.0);
        assert_eq!(g.update(&s), ThermalLevel::Moderate);
        s.skin_c = Some(72.0);
        assert_eq!(g.update(&s), ThermalLevel::Moderate);
        s.skin_c = Some(70.0);
        assert_eq!(g.update(&s), ThermalLevel::Light);
        s.skin_c = Some(60.0);
        assert_eq!(g.update(&s), ThermalLevel::Normal);
    }

    #[test]
    fn severe_caps_hard() {
        let mut g = ThermalGuard::new(Config::default().policy.thermal);
        g.update(&snap(Some(85.0), ThermalLevel::Severe));
        assert_eq!(g.permission(), ThermalPermission::Cap);
        assert_eq!(g.ceiling(), 640);
    }

    #[test]
    fn no_sensor_means_allow() {
        let mut g = ThermalGuard::new(Config::default().policy.thermal);
        assert_eq!(
            g.update(&snap(None, ThermalLevel::Normal)),
            ThermalLevel::Normal
        );
        assert_eq!(g.permission(), ThermalPermission::Allow);
    }
}

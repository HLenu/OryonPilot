//! Policy engine: collects every eligible state, resolves priority, then applies
//! dwell/cooldown gates so the device does not oscillate between profiles.
//!
//! Priority (high to low): SAFE_MODE > THERMAL_GUARD > MEMORY_PRESSURE >
//! BATTERY_SAVER > APP_LAUNCH > GAME > FOREGROUND > BALANCED > IDLE.

use crate::config::{PolicyConfig, Profile};
use crate::types::{PolicyState, ScreenState, Snapshot};

use super::memory_guard::MemoryGuard;
use super::state::StateMachine;
use super::thermal_guard::{ThermalGuard, ThermalPermission};
use super::Decision;

pub struct Engine {
    cfg: PolicyConfig,
    sm: StateMachine,
    thermal: ThermalGuard,
    memory: MemoryGuard,
    profiles: Vec<(PolicyState, Profile)>,
    launchers: Vec<String>,
    last_fg: Option<String>,
    last_fg_change: u64,
    launch_until_ms: Option<u64>,
    screen_off_since_ms: Option<u64>,
    battery_saver: bool,
    safe_mode: bool,
    safe_reason: String,
    last_reason: String,
}

impl Engine {
    pub fn new(
        cfg: PolicyConfig,
        profiles: Vec<(PolicyState, Profile)>,
        launchers: Vec<String>,
        boot_ms: u64,
    ) -> Self {
        Self {
            sm: StateMachine::new(boot_ms, cfg.cooldown_ms),
            thermal: ThermalGuard::new(cfg.thermal.clone()),
            memory: MemoryGuard::new(cfg.memory.clone()),
            cfg,
            profiles,
            launchers,
            last_fg: None,
            last_fg_change: boot_ms,
            launch_until_ms: None,
            screen_off_since_ms: None,
            battery_saver: false,
            safe_mode: false,
            safe_reason: String::new(),
            last_reason: "boot".into(),
        }
    }

    pub fn set_safe_mode(&mut self, on: bool, reason: impl Into<String>) {
        self.safe_mode = on;
        if on {
            self.safe_reason = reason.into();
        }
    }

    pub fn safe_mode(&self) -> bool {
        self.safe_mode
    }

    pub fn machine(&self) -> &StateMachine {
        &self.sm
    }

    /// Hot reload of policy settings; model/state history is preserved.
    pub fn update_config(
        &mut self,
        cfg: PolicyConfig,
        profiles: Vec<(PolicyState, Profile)>,
        launchers: Vec<String>,
    ) {
        self.sm.set_cooldown(cfg.cooldown_ms);
        self.thermal = ThermalGuard::new(cfg.thermal.clone());
        self.memory = MemoryGuard::new(cfg.memory.clone());
        self.launch_until_ms = self.launch_until_ms.map(|deadline| {
            deadline.min(self.last_fg_change.saturating_add(cfg.app_launch.boost_ms))
        });
        self.cfg = cfg;
        self.profiles = profiles;
        self.launchers = launchers;
    }

    pub fn evaluate(&mut self, snap: &Snapshot, now: u64) -> Decision {
        let thermal_level = self.thermal.update(&snap.thermal);
        let mem_class = self.memory.update(&snap.mem, &snap.psi);

        let fg = snap.apps.foreground.clone().unwrap_or_default();
        let valid_foreground = !fg.trim().is_empty()
            && !self.launchers.contains(&fg)
            && snap
                .apps
                .source
                .map(|source| source.trust() > 0.0)
                .unwrap_or(false);
        let screen_on = snap.apps.screen == ScreenState::On;

        if snap.apps.foreground != self.last_fg {
            self.last_fg = snap.apps.foreground.clone();
            self.last_fg_change = now;
            self.launch_until_ms = (valid_foreground && screen_on)
                .then_some(now.saturating_add(self.cfg.app_launch.boost_ms));
        }
        if !valid_foreground || !screen_on {
            self.launch_until_ms = None;
        }
        let launching = self
            .launch_until_ms
            .map(|deadline| now < deadline)
            .unwrap_or(false);
        if !launching {
            self.launch_until_ms = None;
        }
        if snap.apps.screen == ScreenState::Off {
            self.screen_off_since_ms.get_or_insert(now);
        } else {
            self.screen_off_since_ms = None;
        }

        let mut eligible: Vec<(PolicyState, String)> = Vec::new();
        let mut blocked: Vec<String> = Vec::new();

        if self.safe_mode {
            eligible.push((
                PolicyState::SafeMode,
                format!("safe mode: {}", self.safe_reason),
            ));
        }

        let boot_age = now.saturating_sub(self.sm.boot_ms());
        if boot_age < self.cfg.boot_grace_ms {
            eligible.push((
                PolicyState::Boot,
                format!(
                    "boot grace {}s left",
                    (self.cfg.boot_grace_ms - boot_age) / 1000
                ),
            ));
        }

        if self.thermal.is_guarding() {
            eligible.push((
                PolicyState::ThermalGuard,
                self.thermal.describe(&snap.thermal),
            ));
        }

        if self.memory.is_pressured() {
            eligible.push((
                PolicyState::MemoryPressure,
                self.memory.describe(&snap.mem, &snap.psi),
            ));
        }

        let batt = &snap.battery;
        let capacity = batt.capacity_pct;
        if self.cfg.battery.ignore_while_charging && batt.charging {
            self.battery_saver = false;
        } else if let Some(cap) = capacity {
            self.battery_saver = if self.battery_saver {
                cap < self.cfg.battery.exit_pct
            } else {
                cap <= self.cfg.battery.saver_pct
            };
        }
        if self.battery_saver {
            let reason = capacity
                .map(|cap| {
                    format!(
                        "battery {cap:.0}% (enter <= {:.0}%, exit >= {:.0}%)",
                        self.cfg.battery.saver_pct, self.cfg.battery.exit_pct
                    )
                })
                .unwrap_or_else(|| "battery unavailable, retaining saver".into());
            eligible.push((PolicyState::BatterySaver, reason));
        }

        let launch_window = now.saturating_sub(self.last_fg_change);
        if launching {
            eligible.push((
                PolicyState::AppLaunch,
                format!("foreground change {launch_window}ms ago"),
            ));
        }

        if valid_foreground && snap.apps.screen != ScreenState::Off {
            if self.cfg.game.packages.contains(&fg) {
                eligible.push((PolicyState::Game, format!("game package {fg}")));
            } else {
                eligible.push((PolicyState::Foreground, format!("foreground {fg}")));
            }
        }

        if self
            .screen_off_since_ms
            .map(|since| now.saturating_sub(since) >= self.cfg.idle.screen_off_after_ms)
            .unwrap_or(false)
        {
            eligible.push((PolicyState::Idle, "screen off delay elapsed".into()));
        }

        // BALANCED is the fallback: it never outranks a real signal such as IDLE.
        let best = eligible
            .iter()
            .max_by_key(|(s, _)| s.priority())
            .cloned()
            .unwrap_or((PolicyState::Balanced, "default".into()));

        let current = self.sm.current();
        let dwell = self.sm.dwell_ms(now);
        let min_dwell = current.min_dwell_ms().max(self.cfg.min_dwell_ms);
        let mut reason = best.1.clone();

        if best.0 != current {
            let escalating = best.0.priority() > current.priority();
            // Protective states may preempt immediately: waiting out a dwell timer at
            // 60 C or under heavy reclaim is not acceptable.
            let protective = best.0.priority() >= PolicyState::MemoryPressure.priority();
            // Launch boost has a hard deadline. Anti-flapping timers must never
            // prolong it after expiry, screen-off or loss of a valid foreground.
            let launch_finished = current == PolicyState::AppLaunch && !launching;
            let dwell_ok = dwell >= min_dwell || protective || launch_finished;
            let cooldown_ok = escalating || !self.sm.in_cooldown(now) || launch_finished;

            if dwell_ok && cooldown_ok {
                let note = format!("{} -> {}: {}", current.as_str(), best.0.as_str(), best.1);
                self.sm.transition(best.0, now, note);
                self.last_reason = best.1;
            } else {
                if !dwell_ok {
                    blocked.push(format!(
                        "dwell {}ms < {}ms in {}",
                        dwell,
                        min_dwell,
                        current.as_str()
                    ));
                }
                if !cooldown_ok {
                    blocked.push("cooldown active".into());
                }
                reason = format!("holding {}: {}", current.as_str(), self.last_reason);
            }
        } else {
            self.last_reason = best.1.clone();
        }

        let mut profile = self.profile_for(self.sm.current());
        if !screen_on && profile.uclamp_min > 0 {
            blocked.push("screen is not confirmed on, suppressed uclamp.min".into());
            profile.uclamp_min = 0;
        }

        // Thermal and memory permissions can only ever lower the ceiling.
        let ceiling = self.thermal.ceiling();
        if profile.uclamp_max > ceiling {
            profile.uclamp_max = ceiling;
        }
        if self.thermal.permission() != ThermalPermission::Allow && profile.uclamp_min > 0 {
            blocked.push(format!(
                "thermal {} suppressed uclamp.min {}",
                self.thermal.permission().as_str(),
                profile.uclamp_min
            ));
            profile.uclamp_min = 0;
        }
        if self.memory.is_pressured() && profile.uclamp_min > 0 {
            blocked.push("memory pressure suppressed uclamp.min".into());
            profile.uclamp_min = 0;
        }
        // Fail-closed: without a trustworthy CPU temperature reading, no state is
        // allowed to ask for a boost. Capping is left alone (we have no evidence for
        // it), but boost is exactly the action that could cook a device with a dead
        // sensor.
        let trust = snap.thermal.trust;
        if !trust.is_trusted() && profile.uclamp_min > 0 {
            blocked.push(format!(
                "thermal fail-closed ({}): uclamp.min {} suppressed",
                trust.as_str(),
                profile.uclamp_min
            ));
            profile.uclamp_min = 0;
        }
        if self.safe_mode {
            profile = Profile::default();
        }

        Decision {
            state: self.sm.current(),
            reason,
            since_ms: self.sm.since_ms(),
            dwell_ms: dwell,
            eligible,
            thermal_level,
            memory_class: mem_class,
            permission: self.thermal.permission(),
            blocked,
            profile,
            thermal_trust: trust,
            at_ms: now,
        }
    }

    fn profile_for(&self, state: PolicyState) -> Profile {
        self.profiles
            .iter()
            .find(|(s, _)| *s == state)
            .map(|(_, p)| *p)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::types::*;

    fn engine() -> Engine {
        let c = Config::default();
        Engine::new(
            c.policy.clone(),
            c.actuator.profiles.clone(),
            c.apps.launchers.clone(),
            0,
        )
    }

    #[allow(clippy::field_reassign_with_default)]
    fn snapshot(
        fg: Option<&str>,
        screen: ScreenState,
        cpu_temp: Option<f64>,
        batt: f64,
    ) -> Snapshot {
        let mut s = Snapshot::default();
        s.ts_ms = 1_000;
        s.apps.foreground = fg.map(|f| f.to_string());
        s.apps.source = fg.map(|_| AppEventSource::Logcat);
        s.apps.screen = screen;
        s.battery.capacity_pct = Some(batt);
        s.thermal.cpu_max_c = cpu_temp;
        s.thermal.cpu_c = cpu_temp;
        s.thermal.trust = ThermalTrust::Trusted;
        s.thermal.level = Some(match cpu_temp {
            Some(t) if t >= 83.0 => ThermalLevel::Severe,
            Some(t) if t >= 75.0 => ThermalLevel::Moderate,
            Some(t) if t >= 65.0 => ThermalLevel::Light,
            _ => ThermalLevel::Normal,
        });
        s.mem.total_kb = 11_534_336.0;
        s.mem.available_kb = 5_000_000.0;
        s.mem.meminfo_ok = true;
        s
    }

    #[test]
    fn boot_state_holds_for_grace_period() {
        let mut e = engine();
        let d = e.evaluate(
            &snapshot(Some("com.a"), ScreenState::On, Some(40.0), 80.0),
            1_000,
        );
        assert_eq!(d.state, PolicyState::Boot);
    }

    #[test]
    fn foreground_beats_balanced_after_boot() {
        let mut e = engine();
        let s = snapshot(Some("com.a"), ScreenState::On, Some(40.0), 80.0);
        e.evaluate(&s, 1_000);
        let d = e.evaluate(&s, 100_000);
        assert_eq!(d.state, PolicyState::Foreground);
        assert_eq!(d.profile.uclamp_min, 128);
    }

    #[test]
    fn thermal_guard_preempts_and_caps() {
        let mut e = engine();
        e.evaluate(
            &snapshot(Some("com.a"), ScreenState::On, Some(40.0), 80.0),
            1_000,
        );
        let d = e.evaluate(
            &snapshot(Some("com.a"), ScreenState::On, Some(85.0), 80.0),
            100_000,
        );
        assert_eq!(d.state, PolicyState::ThermalGuard);
        assert!(d.profile.uclamp_max <= 640);
        assert_eq!(d.profile.uclamp_min, 0);
    }

    #[test]
    fn memory_pressure_never_raises_min() {
        let mut e = engine();
        let mut s = snapshot(Some("com.a"), ScreenState::On, Some(40.0), 80.0);
        s.mem.available_kb = 300_000.0;
        s.mem.kswapd_cpu_pct = 60.0;
        s.psi.memory.some10 = 30.0;
        s.psi.memory.full10 = 9.0;
        s.psi.memory.available = true;
        e.evaluate(&s, 1_000);
        let d = e.evaluate(&s, 100_000);
        assert_eq!(d.state, PolicyState::MemoryPressure);
        assert_eq!(d.profile.uclamp_min, 0);
        assert!(d.profile.uclamp_max <= 768);
    }

    #[test]
    fn battery_saver_engages_at_threshold() {
        let mut e = engine();
        e.evaluate(
            &snapshot(Some("com.a"), ScreenState::On, Some(40.0), 80.0),
            1_000,
        );
        let d = e.evaluate(
            &snapshot(Some("com.a"), ScreenState::On, Some(40.0), 10.0),
            100_000,
        );
        assert_eq!(d.state, PolicyState::BatterySaver);
    }

    #[test]
    fn idle_when_screen_off() {
        let mut e = engine();
        e.evaluate(
            &snapshot(Some("com.a"), ScreenState::On, Some(40.0), 80.0),
            1_000,
        );
        e.evaluate(
            &snapshot(Some("com.a"), ScreenState::Off, Some(40.0), 80.0),
            90_000,
        );
        let d = e.evaluate(
            &snapshot(Some("com.a"), ScreenState::Off, Some(40.0), 80.0),
            100_000,
        );
        assert_eq!(d.state, PolicyState::Idle);
    }

    #[test]
    fn safe_mode_wins_over_everything() {
        let mut e = engine();
        e.set_safe_mode(true, "test");
        let d = e.evaluate(
            &snapshot(Some("com.a"), ScreenState::On, Some(85.0), 80.0),
            100_000,
        );
        assert_eq!(d.state, PolicyState::SafeMode);
        assert_eq!(d.profile, Profile::default());
    }

    #[test]
    fn dwell_prevents_fast_flapping() {
        let mut e = engine();
        e.evaluate(
            &snapshot(Some("com.a"), ScreenState::On, Some(40.0), 80.0),
            1_000,
        );
        e.evaluate(
            &snapshot(Some("com.a"), ScreenState::On, Some(40.0), 80.0),
            100_000,
        );
        // Drop to screen-off: must hold FOREGROUND for min dwell.
        let d = e.evaluate(
            &snapshot(Some("com.a"), ScreenState::Off, Some(40.0), 80.0),
            101_000,
        );
        assert_eq!(d.state, PolicyState::Foreground);
        assert!(!d.blocked.is_empty());
        let d2 = e.evaluate(
            &snapshot(Some("com.a"), ScreenState::Off, Some(40.0), 80.0),
            120_000,
        );
        assert_eq!(d2.state, PolicyState::Idle);
    }

    #[test]
    fn launch_deadline_overrides_dwell_and_cooldown() {
        let mut cfg = Config::default();
        cfg.policy.min_dwell_ms = 30_000;
        cfg.policy.cooldown_ms = 60_000;
        let mut e = Engine::new(cfg.policy, cfg.actuator.profiles, cfg.apps.launchers, 0);
        e.evaluate(&snapshot(None, ScreenState::On, Some(40.0), 80.0), 100_000);
        let s = snapshot(Some("com.example.app"), ScreenState::On, Some(40.0), 80.0);
        assert_eq!(e.evaluate(&s, 200_000).state, PolicyState::AppLaunch);
        assert_eq!(e.evaluate(&s, 202_499).state, PolicyState::AppLaunch);
        let expired = e.evaluate(&s, 202_500);
        assert_eq!(expired.state, PolicyState::Foreground);
        assert_eq!(expired.profile.uclamp_min, 128);
        assert!(!expired
            .blocked
            .iter()
            .any(|reason| reason.contains("cooldown")));
    }

    #[test]
    fn launch_ends_immediately_on_screen_off_even_for_a_game() {
        let mut cfg = Config::default();
        cfg.policy.game.packages.push("com.example.game".into());
        let mut e = Engine::new(cfg.policy, cfg.actuator.profiles, cfg.apps.launchers, 0);
        let mut s = snapshot(Some("com.example.game"), ScreenState::On, Some(40.0), 80.0);
        assert_eq!(e.evaluate(&s, 200_000).state, PolicyState::AppLaunch);
        s.apps.screen = ScreenState::Off;
        let off = e.evaluate(&s, 200_001);
        assert_eq!(off.state, PolicyState::Balanced);
        assert_eq!(off.profile.uclamp_min, 0);
        assert!(!off
            .eligible
            .iter()
            .any(|(state, _)| matches!(state, PolicyState::Game | PolicyState::AppLaunch)));
    }

    #[test]
    fn config_reload_can_shorten_but_cannot_extend_an_active_launch() {
        for boost_ms in [1_000, 10_000] {
            let mut e = engine();
            let s = snapshot(Some("com.example.app"), ScreenState::On, Some(40.0), 80.0);
            assert_eq!(e.evaluate(&s, 200_000).state, PolicyState::AppLaunch);
            let mut cfg = Config::default();
            cfg.policy.app_launch.boost_ms = boost_ms;
            e.update_config(cfg.policy, cfg.actuator.profiles, cfg.apps.launchers);
            let deadline = 200_000 + boost_ms.min(2_500);
            assert_eq!(e.evaluate(&s, deadline - 1).state, PolicyState::AppLaunch);
            assert_eq!(e.evaluate(&s, deadline).state, PolicyState::Foreground);
        }
    }

    #[test]
    fn missing_launcher_or_untrusted_foreground_cannot_trigger_launch() {
        for fg in [None, Some(""), Some("com.miui.home")] {
            let mut e = engine();
            let d = e.evaluate(&snapshot(fg, ScreenState::On, Some(40.0), 80.0), 1_000);
            assert!(
                !d.eligible
                    .iter()
                    .any(|(state, _)| *state == PolicyState::AppLaunch),
                "{fg:?}"
            );
        }
        let mut e = engine();
        let mut s = snapshot(Some("com.example.app"), ScreenState::On, Some(40.0), 80.0);
        s.apps.source = None;
        assert!(!e
            .evaluate(&s, 200_000)
            .eligible
            .iter()
            .any(|(state, _)| *state == PolicyState::AppLaunch));
        s.apps.source = Some(AppEventSource::None);
        assert!(!e
            .evaluate(&s, 210_000)
            .eligible
            .iter()
            .any(|(state, _)| *state == PolicyState::AppLaunch));
    }

    #[test]
    fn loss_of_foreground_ends_an_active_launch_immediately() {
        let mut e = engine();
        let mut s = snapshot(Some("com.example.app"), ScreenState::On, Some(40.0), 80.0);
        assert_eq!(e.evaluate(&s, 200_000).state, PolicyState::AppLaunch);
        s.apps.foreground = None;
        s.apps.source = None;
        let d = e.evaluate(&s, 200_001);
        assert_eq!(d.state, PolicyState::Balanced);
        assert_eq!(d.profile.uclamp_min, 0);
    }

    #[test]
    fn battery_saver_uses_separate_entry_and_exit_thresholds() {
        let mut e = engine();
        let mut s = snapshot(Some("com.example.app"), ScreenState::On, Some(40.0), 15.0);
        assert_eq!(e.evaluate(&s, 120_000).state, PolicyState::BatterySaver);
        s.battery.capacity_pct = Some(18.0);
        assert_eq!(e.evaluate(&s, 140_000).state, PolicyState::BatterySaver);
        s.battery.capacity_pct = Some(19.9);
        assert_eq!(e.evaluate(&s, 160_000).state, PolicyState::BatterySaver);
        s.battery.capacity_pct = Some(20.0);
        assert_eq!(e.evaluate(&s, 180_000).state, PolicyState::Foreground);
        s.battery.capacity_pct = Some(18.0);
        assert_eq!(e.evaluate(&s, 190_000).state, PolicyState::Foreground);
        s.battery.capacity_pct = Some(15.0);
        assert_eq!(e.evaluate(&s, 200_000).state, PolicyState::BatterySaver);
        s.battery.charging = true;
        assert_eq!(e.evaluate(&s, 220_000).state, PolicyState::Foreground);
    }

    #[test]
    fn idle_waits_for_the_configured_continuous_screen_off_interval() {
        let mut cfg = Config::default();
        cfg.policy.idle.screen_off_after_ms = 7_000;
        let mut e = Engine::new(cfg.policy, cfg.actuator.profiles, cfg.apps.launchers, 0);
        let mut s = snapshot(None, ScreenState::On, Some(40.0), 80.0);
        assert_eq!(e.evaluate(&s, 100_000).state, PolicyState::Balanced);
        s.apps.screen = ScreenState::Off;
        assert_eq!(e.evaluate(&s, 200_000).state, PolicyState::Balanced);
        assert_eq!(e.evaluate(&s, 206_999).state, PolicyState::Balanced);
        s.apps.screen = ScreenState::On;
        e.evaluate(&s, 207_000);
        s.apps.screen = ScreenState::Off;
        assert_eq!(e.evaluate(&s, 208_000).state, PolicyState::Balanced);
        assert_eq!(e.evaluate(&s, 214_999).state, PolicyState::Balanced);
        assert_eq!(e.evaluate(&s, 215_000).state, PolicyState::Idle);
    }
}

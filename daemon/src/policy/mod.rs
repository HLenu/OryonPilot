//! Policy layer: one state machine, two guards, one decision per tick.

pub mod engine;
pub mod memory_guard;
pub mod state;
pub mod thermal_guard;

pub use engine::Engine;
pub use memory_guard::MemoryClass;
pub use state::Transition;
pub use thermal_guard::{ThermalGuard, ThermalPermission};

use crate::config::Profile;
use crate::json::Json;
use crate::types::{PolicyState, ThermalLevel, ThermalTrust};

#[derive(Debug, Clone)]
pub struct Decision {
    pub state: PolicyState,
    pub reason: String,
    pub since_ms: u64,
    pub dwell_ms: u64,
    /// Every state that was eligible this tick, with the reason it was eligible.
    pub eligible: Vec<(PolicyState, String)>,
    pub thermal_level: ThermalLevel,
    pub memory_class: MemoryClass,
    pub permission: ThermalPermission,
    /// Reasons a transition or a write was suppressed. Empty means "nothing blocked".
    pub blocked: Vec<String>,
    pub profile: Profile,
    /// Sensor trust at decision time; anything but `Trusted` forbids boost.
    pub thermal_trust: ThermalTrust,
    pub at_ms: u64,
}

impl Default for Decision {
    fn default() -> Self {
        Self {
            state: PolicyState::Boot,
            reason: "init".into(),
            since_ms: 0,
            dwell_ms: 0,
            eligible: Vec::new(),
            thermal_level: ThermalLevel::Normal,
            memory_class: MemoryClass::Normal,
            permission: ThermalPermission::Allow,
            blocked: Vec::new(),
            profile: Profile::default(),
            thermal_trust: ThermalTrust::Unknown,
            at_ms: 0,
        }
    }
}

impl Decision {
    pub fn to_json(&self) -> Json {
        let mut j = Json::obj();
        j.set("state", Json::str(self.state.as_str()));
        j.set("reason", Json::str(self.reason.clone()));
        j.set("since_ms", Json::num(self.since_ms as f64));
        j.set("dwell_ms", Json::num(self.dwell_ms as f64));
        j.set("thermal_level", Json::str(self.thermal_level.as_str()));
        j.set("memory_class", Json::str(self.memory_class.as_str()));
        j.set("permission", Json::str(self.permission.as_str()));
        j.set("thermal_trust", Json::str(self.thermal_trust.as_str()));
        let mut p = Json::obj();
        p.set("uclamp_min", Json::num(self.profile.uclamp_min as f64));
        p.set("uclamp_max", Json::num(self.profile.uclamp_max as f64));
        j.set("profile", p);
        j.set(
            "blocked",
            Json::Arr(self.blocked.iter().cloned().map(Json::Str).collect()),
        );
        j.set(
            "eligible",
            Json::Arr(
                self.eligible
                    .iter()
                    .map(|(s, why)| {
                        let mut o = Json::obj();
                        o.set("state", Json::str(s.as_str()));
                        o.set("reason", Json::str(why.clone()));
                        o.set("priority", Json::num(s.priority() as f64));
                        o
                    })
                    .collect(),
            ),
        );
        j
    }

    /// Maps the policy state to the 5 canonical scheduling actions for shadow comparison.
    pub fn sched_action(&self) -> crate::learner::bandit::SchedAction {
        use crate::learner::bandit::SchedAction;
        match self.state {
            PolicyState::ThermalGuard | PolicyState::MemoryPressure => SchedAction::ThermalCap,
            PolicyState::BatterySaver | PolicyState::Idle => SchedAction::DeepSave,
            PolicyState::AppLaunch | PolicyState::Game => SchedAction::Boost,
            PolicyState::Foreground => SchedAction::Responsive,
            PolicyState::Balanced | PolicyState::Boot | PolicyState::SafeMode => {
                SchedAction::Efficiency
            }
        }
    }

    /// True when policy guards forbid boosting and require thermal/memory capping.
    pub fn requires_protective_cap(&self) -> bool {
        self.permission >= ThermalPermission::NoBoost
            || self.memory_class != MemoryClass::Normal
            || matches!(
                self.state,
                PolicyState::ThermalGuard | PolicyState::MemoryPressure
            )
    }
}

//! Residual Contextual LinUCB Bandit + Delayed 60s Shadow Evaluator.
//!
//! Uses exact Sherman-Morrison incremental matrix inverse updates over 15-dim state context,
//! warm-started with zero-mean Offline RL FQI advantages (`1.5 * (Q_fqi - mean(Q_fqi))`).

use super::gbdt::{dot15, CONTEXT_DIM, N_ACTIONS};
use crate::json::Json;

pub const INIT_LAMBDA: f64 = 8.0;
pub const UCB_ALPHA: f64 = 0.15;
pub const FORGET_GAMMA: f64 = 0.999;
const MAX_A_INV_DIAG: f64 = 10.0;
const FQI_ADV_SCALE: f64 = 1.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SchedAction {
    DeepSave = 0,
    #[default]
    Efficiency = 1,
    Responsive = 2,
    Boost = 3,
    ThermalCap = 4,
}

impl SchedAction {
    pub fn from_index(idx: usize) -> Self {
        match idx {
            0 => SchedAction::DeepSave,
            1 => SchedAction::Efficiency,
            2 => SchedAction::Responsive,
            3 => SchedAction::Boost,
            4 => SchedAction::ThermalCap,
            _ => SchedAction::Efficiency,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SchedAction::DeepSave => "DEEP_SAVE",
            SchedAction::Efficiency => "EFFICIENCY",
            SchedAction::Responsive => "RESPONSIVE",
            SchedAction::Boost => "BOOST",
            SchedAction::ThermalCap => "THERMAL_CAP",
        }
    }

    pub fn uclamp(self) -> (u32, u32) {
        match self {
            SchedAction::DeepSave => (0, 512),
            SchedAction::Efficiency => (0, 768),
            SchedAction::Responsive => (128, 1024),
            SchedAction::Boost => (320, 1024),
            SchedAction::ThermalCap => (0, 640),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PolicyGuardInput {
    pub temp_c: f64,
    pub psi_full10: f64,
    pub mem_avail_pct: f64,
    pub overheat_prob: f64,
    pub th_overheat: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct StaticRuleInput {
    pub cpu_pct: f64,
    pub prime_mhz: f64,
    pub temp_c: f64,
    pub psi_full10: f64,
    pub mem_avail_pct: f64,
    pub is_launcher: bool,
    pub is_charging: bool,
    pub batt_pct: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct RewardObs {
    pub act: SchedAction,
    pub prev_act: SchedAction,
    pub cpu_next: f64,
    pub prime_next_mhz: f64,
    pub temp_c: f64,
    pub psi_full10: f64,
    pub mem_avail_pct: f64,
    pub is_launcher: bool,
    pub is_charging: bool,
    pub batt_pct: f64,
}

#[derive(Debug, Clone)]
pub struct ResidualLinUcb {
    pub a_inv: [[[f64; CONTEXT_DIM]; CONTEXT_DIM]; N_ACTIONS],
    pub b: [[f64; CONTEXT_DIM]; N_ACTIONS],
}

impl Default for ResidualLinUcb {
    fn default() -> Self {
        let mut a_inv = [[[0.0f64; CONTEXT_DIM]; CONTEXT_DIM]; N_ACTIONS];
        let diag = 1.0 / INIT_LAMBDA;
        for mat in &mut a_inv {
            for (i, row) in mat.iter_mut().enumerate() {
                row[i] = diag;
            }
        }
        Self {
            a_inv,
            b: [[0.0f64; CONTEXT_DIM]; N_ACTIONS],
        }
    }
}

impl ResidualLinUcb {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn select(
        &self,
        x: &[f64; CONTEXT_DIM],
        fqi_q: &[f64; N_ACTIONS],
        guard: PolicyGuardInput,
    ) -> (SchedAction, [f64; N_ACTIONS], &'static str) {
        let q_mean = fqi_q.iter().sum::<f64>() / N_ACTIONS as f64;
        let mut scores = [0.0f64; N_ACTIONS];
        for (a, score_slot) in scores.iter_mut().enumerate() {
            let theta = mat_vec15(&self.a_inv[a], &self.b[a]);
            let mean = dot15(&theta, x);
            let ax = mat_vec15(&self.a_inv[a], x);
            let var = dot15(x, &ax).max(0.0);
            let adv = (fqi_q[a] - q_mean) * FQI_ADV_SCALE;
            *score_slot = adv + mean + UCB_ALPHA * var.sqrt();
        }

        if guard.temp_c >= 78.0
            || guard.psi_full10 >= 3.0
            || guard.mem_avail_pct < 8.0
            || (guard.overheat_prob >= guard.th_overheat && guard.temp_c >= 65.0)
        {
            return (SchedAction::ThermalCap, scores, "safety_override");
        }

        let mut best_a = 1usize;
        let mut best_s = f64::NEG_INFINITY;
        for (a, &s) in scores.iter().enumerate() {
            if s > best_s {
                best_s = s;
                best_a = a;
            }
        }
        (SchedAction::from_index(best_a), scores, "fqi_linucb")
    }

    /// Sherman-Morrison exact rank-1 update with forgetting stabilization.
    pub fn update(&mut self, x: &[f64; CONTEXT_DIM], action: SchedAction, reward: f64) {
        if !reward.is_finite() || x.iter().any(|v| !v.is_finite()) {
            return;
        }
        let inv_gamma = 1.0 / FORGET_GAMMA;
        for k in 0..N_ACTIONS {
            // Only rescale if max diagonal stays well within numerical safety cap.
            let max_diag = (0..CONTEXT_DIM)
                .map(|i| self.a_inv[k][i][i])
                .fold(0.0f64, f64::max);
            if max_diag * inv_gamma <= MAX_A_INV_DIAG {
                for row in &mut self.a_inv[k] {
                    for v in row.iter_mut() {
                        *v *= inv_gamma;
                    }
                }
            }
            for v in &mut self.b[k] {
                *v *= FORGET_GAMMA;
            }
        }

        let a = action as usize;
        let v = mat_vec15(&self.a_inv[a], x);
        let denom = 1.0 + dot15(x, &v);
        if denom > 1e-12 {
            let scale = 1.0 / denom;
            for (row, &vi) in self.a_inv[a].iter_mut().zip(v.iter()) {
                for (cell, &vj) in row.iter_mut().zip(v.iter()) {
                    *cell -= vi * vj * scale;
                }
            }
        }
        for (b_slot, &xi) in self.b[a].iter_mut().zip(x.iter()) {
            *b_slot += reward * xi;
        }
    }

    pub fn write_exact_json(&self, out: &mut String) {
        use std::fmt::Write as _;
        out.push_str("{\"a_inv\":[");
        for (ai, mat) in self.a_inv.iter().enumerate() {
            if ai > 0 {
                out.push(',');
            }
            out.push('[');
            for (ri, row) in mat.iter().enumerate() {
                if ri > 0 {
                    out.push(',');
                }
                out.push('[');
                for (ci, val) in row.iter().enumerate() {
                    if ci > 0 {
                        out.push(',');
                    }
                    let _ = write!(out, "{val}");
                }
                out.push(']');
            }
            out.push(']');
        }
        out.push_str("],\"b\":[");
        for (ai, row) in self.b.iter().enumerate() {
            if ai > 0 {
                out.push(',');
            }
            out.push('[');
            for (ci, val) in row.iter().enumerate() {
                if ci > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{val}");
            }
            out.push(']');
        }
        out.push_str("]}");
    }

    pub fn from_json(j: &Json) -> Option<Self> {
        let a_arr = j.get("a_inv")?.as_arr();
        let b_arr = j.get("b")?.as_arr();
        if a_arr.len() != N_ACTIONS || b_arr.len() != N_ACTIONS {
            return None;
        }
        let mut out = Self::default();
        for a in 0..N_ACTIONS {
            let mat = a_arr[a].as_arr();
            let brow = b_arr[a].as_arr();
            if mat.len() != CONTEXT_DIM || brow.len() != CONTEXT_DIM {
                return None;
            }
            for i in 0..CONTEXT_DIM {
                out.b[a][i] = brow[i].as_f64()?;
                let r = mat[i].as_arr();
                if r.len() != CONTEXT_DIM {
                    return None;
                }
                for (c, cell) in r.iter().enumerate() {
                    out.a_inv[a][i][c] = cell.as_f64()?;
                }
            }
        }
        Some(out)
    }
}

pub fn static_rule_action(inp: StaticRuleInput) -> SchedAction {
    if inp.temp_c >= 75.0 || inp.psi_full10 >= 3.0 || inp.mem_avail_pct < 8.0 {
        return SchedAction::ThermalCap;
    }
    if (!inp.is_charging && inp.batt_pct <= 15.0)
        || (inp.is_launcher && inp.cpu_pct < 4.5 && inp.prime_mhz <= 1200.0)
    {
        return SchedAction::DeepSave;
    }
    if inp.cpu_pct >= 25.0 || inp.prime_mhz >= 2400.0 {
        return SchedAction::Boost;
    }
    if !inp.is_launcher && inp.cpu_pct >= 5.0 {
        return SchedAction::Responsive;
    }
    SchedAction::Efficiency
}

pub fn compute_step_reward(obs: RewardObs) -> f64 {
    let (umin, umax) = obs.act.uclamp();
    let cap_ratio = umax as f64 / 1024.0;
    let floor_ratio = umin as f64 / 1024.0;

    let mut demand = (obs.cpu_next / 40.0)
        .max((obs.prime_next_mhz - 1000.0) / 3000.0)
        .clamp(0.0, 1.2);
    if obs.psi_full10 >= 3.0 || obs.mem_avail_pct < 8.0 {
        demand = demand.min(0.75);
    }

    let perf_score = if cap_ratio < demand {
        let shortfall = demand - cap_ratio;
        let mult = if obs.temp_c < 75.0 { 1.0 } else { 0.35 };
        -2.2 * shortfall * mult
    } else {
        let mut s = 0.65 * demand.min(1.0);
        if demand >= 0.55 && floor_ratio >= 0.25 && obs.temp_c < 72.0 {
            s += 0.45;
        } else if demand >= 0.25 && floor_ratio >= 0.10 && obs.temp_c < 75.0 {
            s += 0.20;
        }
        s
    };

    let mut thermal_pen = 0.0;
    if obs.temp_c >= 72.0 {
        thermal_pen += 0.09 * (obs.temp_c - 72.0);
    }
    if obs.temp_c >= 78.0 {
        thermal_pen += 0.22 * (obs.temp_c - 78.0);
    }
    if obs.temp_c >= 85.0 {
        thermal_pen += 0.45 * (obs.temp_c - 85.0);
    }

    let mut waste_pen = 0.0;
    if demand < 0.20 {
        waste_pen += 0.85 * floor_ratio + 0.28 * (cap_ratio - 0.50).max(0.0);
    } else if demand < 0.40 {
        waste_pen += 0.40 * (floor_ratio - 0.125).max(0.0) + 0.12 * (cap_ratio - 0.75).max(0.0);
    }
    if obs.is_launcher && demand < 0.25 {
        waste_pen += 0.30 * floor_ratio + 0.15 * (cap_ratio - 0.55).max(0.0);
    }
    if !obs.is_charging && obs.batt_pct <= 20.0 {
        waste_pen += 0.25 * (cap_ratio - 0.50).max(0.0);
    }

    let switch_pen = if obs.act != obs.prev_act { 0.08 } else { 0.0 };
    perf_score - thermal_pen - waste_pen - switch_pen
}

#[derive(Debug, Clone)]
pub struct PendingShadowStep {
    pub context_15: [f64; CONTEXT_DIM],
    pub chosen: SchedAction,
    pub static_act: SchedAction,
    pub prev_chosen: SchedAction,
    pub prev_static: SchedAction,
    pub was_is_launcher: bool,
    pub was_is_charging: bool,
    pub was_batt_pct: f64,
    pub was_psi_full: f64,
    pub was_mem_pct: f64,
}

#[derive(Debug, Clone, Default)]
pub struct ShadowEvaluator {
    pub updates: u64,
    pub learned_reward_sum: f64,
    pub static_reward_sum: f64,
    pub shadow_wins: u64,
    pub shadow_ties: u64,
    pub shadow_losses: u64,
    pub action_counts: [u64; N_ACTIONS],
    pub wasted_boost_mins: u64,
    pub thermal_spike_mins: u64,
    pub memory_pressure_mins: u64,
    prev_chosen: SchedAction,
    prev_static: SchedAction,
    pending: Option<PendingShadowStep>,
}

impl ShadowEvaluator {
    pub fn reset(&mut self) {
        *self = Self {
            prev_chosen: SchedAction::Efficiency,
            prev_static: SchedAction::Efficiency,
            ..Default::default()
        };
    }

    pub fn reward_lift(&self) -> f64 {
        if self.updates == 0 {
            0.0
        } else {
            (self.learned_reward_sum - self.static_reward_sum) / self.updates as f64
        }
    }

    pub fn learned_mean_reward(&self) -> f64 {
        if self.updates == 0 {
            0.0
        } else {
            self.learned_reward_sum / self.updates as f64
        }
    }

    pub fn static_mean_reward(&self) -> f64 {
        if self.updates == 0 {
            0.0
        } else {
            self.static_reward_sum / self.updates as f64
        }
    }

    pub fn shadow_win_rate(&self) -> f64 {
        let decisive = self.shadow_wins + self.shadow_losses;
        if decisive == 0 {
            if self.updates > 0 {
                0.5
            } else {
                0.0
            }
        } else {
            self.shadow_wins as f64 / decisive as f64
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn on_minute_step(
        &mut self,
        bandit: &mut ResidualLinUcb,
        context_15: [f64; CONTEXT_DIM],
        chosen: SchedAction,
        static_act: SchedAction,
        cpu_now: f64,
        prime_now_mhz: f64,
        temp_now_c: f64,
        psi_full_now: f64,
        mem_pct_now: f64,
        is_launcher_now: bool,
        is_charging_now: bool,
        batt_pct_now: f64,
    ) {
        // Track auto-discovered runtime issues cleanly from real 1-minute telemetry.
        if cpu_now < 12.0 && prime_now_mhz >= 2000.0 {
            self.wasted_boost_mins = self.wasted_boost_mins.saturating_add(1);
        }
        if temp_now_c >= 75.0 {
            self.thermal_spike_mins = self.thermal_spike_mins.saturating_add(1);
        }
        if psi_full_now >= 3.0 || mem_pct_now < 8.0 {
            self.memory_pressure_mins = self.memory_pressure_mins.saturating_add(1);
        }

        if let Some(p) = self.pending.take() {
            let r_learned = compute_step_reward(RewardObs {
                act: p.chosen,
                prev_act: p.prev_chosen,
                cpu_next: cpu_now,
                prime_next_mhz: prime_now_mhz,
                temp_c: temp_now_c,
                psi_full10: p.was_psi_full,
                mem_avail_pct: p.was_mem_pct,
                is_launcher: p.was_is_launcher,
                is_charging: p.was_is_charging,
                batt_pct: p.was_batt_pct,
            });
            let r_static = compute_step_reward(RewardObs {
                act: p.static_act,
                prev_act: p.prev_static,
                cpu_next: cpu_now,
                prime_next_mhz: prime_now_mhz,
                temp_c: temp_now_c,
                psi_full10: p.was_psi_full,
                mem_avail_pct: p.was_mem_pct,
                is_launcher: p.was_is_launcher,
                is_charging: p.was_is_charging,
                batt_pct: p.was_batt_pct,
            });

            bandit.update(&p.context_15, p.chosen, r_learned);

            self.updates = self.updates.saturating_add(1);
            self.learned_reward_sum += r_learned;
            self.static_reward_sum += r_static;
            let diff = r_learned - r_static;
            if diff > 1e-4 {
                self.shadow_wins = self.shadow_wins.saturating_add(1);
            } else if diff < -1e-4 {
                self.shadow_losses = self.shadow_losses.saturating_add(1);
            } else {
                self.shadow_ties = self.shadow_ties.saturating_add(1);
            }
            self.action_counts[p.chosen as usize] =
                self.action_counts[p.chosen as usize].saturating_add(1);
        }

        let prev_c = self.prev_chosen;
        let prev_s = self.prev_static;
        self.prev_chosen = chosen;
        self.prev_static = static_act;
        self.pending = Some(PendingShadowStep {
            context_15,
            chosen,
            static_act,
            prev_chosen: prev_c,
            prev_static: prev_s,
            was_is_launcher: is_launcher_now,
            was_is_charging: is_charging_now,
            was_batt_pct: batt_pct_now,
            was_psi_full: psi_full_now,
            was_mem_pct: mem_pct_now,
        });
    }

    pub fn write_exact_json(&self, out: &mut String) {
        use std::fmt::Write as _;
        let _ = write!(
            out,
            "{{\"updates\":{},\"learned_reward_sum\":{},\"static_reward_sum\":{},\"shadow_wins\":{},\"shadow_ties\":{},\"shadow_losses\":{},\"action_counts\":[{},{},{},{},{}],\"wasted_boost_mins\":{},\"thermal_spike_mins\":{},\"memory_pressure_mins\":{}}}",
            self.updates,
            self.learned_reward_sum,
            self.static_reward_sum,
            self.shadow_wins,
            self.shadow_ties,
            self.shadow_losses,
            self.action_counts[0],
            self.action_counts[1],
            self.action_counts[2],
            self.action_counts[3],
            self.action_counts[4],
            self.wasted_boost_mins,
            self.thermal_spike_mins,
            self.memory_pressure_mins
        );
    }

    pub fn from_json(j: &Json) -> Self {
        let mut counts = [0u64; N_ACTIONS];
        if let Some(arr) = j.get("action_counts").map(Json::as_arr) {
            for (i, v) in arr.iter().take(N_ACTIONS).enumerate() {
                counts[i] = v.as_u64().unwrap_or(0);
            }
        }
        Self {
            updates: j.get("updates").and_then(Json::as_u64).unwrap_or(0),
            learned_reward_sum: j
                .get("learned_reward_sum")
                .and_then(Json::as_f64)
                .unwrap_or(0.0),
            static_reward_sum: j
                .get("static_reward_sum")
                .and_then(Json::as_f64)
                .unwrap_or(0.0),
            shadow_wins: j.get("shadow_wins").and_then(Json::as_u64).unwrap_or(0),
            shadow_ties: j.get("shadow_ties").and_then(Json::as_u64).unwrap_or(0),
            shadow_losses: j.get("shadow_losses").and_then(Json::as_u64).unwrap_or(0),
            action_counts: counts,
            wasted_boost_mins: j
                .get("wasted_boost_mins")
                .and_then(Json::as_u64)
                .unwrap_or(0),
            thermal_spike_mins: j
                .get("thermal_spike_mins")
                .and_then(Json::as_u64)
                .unwrap_or(0),
            memory_pressure_mins: j
                .get("memory_pressure_mins")
                .and_then(Json::as_u64)
                .unwrap_or(0),
            prev_chosen: SchedAction::Efficiency,
            prev_static: SchedAction::Efficiency,
            pending: None,
        }
    }
}

#[inline]
fn mat_vec15(
    mat: &[[f64; CONTEXT_DIM]; CONTEXT_DIM],
    v: &[f64; CONTEXT_DIM],
) -> [f64; CONTEXT_DIM] {
    let mut out = [0.0f64; CONTEXT_DIM];
    for (slot, row) in out.iter_mut().zip(mat.iter()) {
        *slot = dot15(row, v);
    }
    out
}

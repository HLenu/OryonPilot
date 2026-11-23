//! Dual-Loop Self-Improving Scheduler Learner (Pure Rust `std`).
//!
//! Fast Loop (1 Hz): runs embedded/hot-loaded `CompactGbdtModel` + `ResidualLinUcb`
//! inference to forecast next-minute CPU%, burst probability, +1m/+3m temperature,
//! 3-minute overheat probability, FQI Q-values, and recommended `(uclamp_min, uclamp_max)`.
//!
//! Shadow & Adaptation Loop (60s cadence): settles delayed outcomes against actual
//! observed device state, computes multi-objective reward for both the learned policy
//! and static rules, and updates `ResidualLinUcb` online via Sherman-Morrison.

pub mod bandit;
pub mod gbdt;

use crate::json::{self, Json};
use crate::storage::secure_file;
use crate::types::Snapshot;
use bandit::{PolicyGuardInput, ResidualLinUcb, SchedAction, ShadowEvaluator, StaticRuleInput};
use gbdt::{CompactGbdtModel, FeatureExtractor, ModelSource, N_ACTIONS};
use std::path::Path;

const STATE_SCHEMA: u64 = 1;
const MINUTE_CADENCE_MS: u64 = 60_000;

#[derive(Debug, Clone)]
pub struct LearnerSnapshot {
    pub model_source: &'static str,
    pub architecture: String,
    pub pred_cpu_pct: f64,
    pub burst_prob: f64,
    pub pred_t1_c: f64,
    pub pred_t3_c: f64,
    pub overheat_3m_prob: f64,
    pub th_burst: f64,
    pub th_overheat: f64,
    pub recommended_action: SchedAction,
    pub static_action: SchedAction,
    pub decision_source: &'static str,
    pub uclamp_min: u32,
    pub uclamp_max: u32,
    pub action_probs: [f64; N_ACTIONS],
    pub fqi_q: [f64; N_ACTIONS],
    pub bandit_scores: [f64; N_ACTIONS],
    pub updates: u64,
    pub learned_mean_reward: f64,
    pub static_mean_reward: f64,
    pub reward_lift: f64,
    pub shadow_wins: u64,
    pub shadow_ties: u64,
    pub shadow_losses: u64,
    pub shadow_win_rate: f64,
    pub action_counts: [u64; N_ACTIONS],
    pub wasted_boost_mins: u64,
    pub thermal_spike_mins: u64,
    pub memory_pressure_mins: u64,
}

impl Default for LearnerSnapshot {
    fn default() -> Self {
        let (umin, umax) = SchedAction::Efficiency.uclamp();
        Self {
            model_source: "embedded",
            architecture: "CompactGBDT_FQI".to_string(),
            pred_cpu_pct: 0.0,
            burst_prob: 0.0,
            pred_t1_c: 0.0,
            pred_t3_c: 0.0,
            overheat_3m_prob: 0.0,
            th_burst: 0.5,
            th_overheat: 0.25,
            recommended_action: SchedAction::Efficiency,
            static_action: SchedAction::Efficiency,
            decision_source: "init",
            uclamp_min: umin,
            uclamp_max: umax,
            action_probs: [0.0; N_ACTIONS],
            fqi_q: [0.0; N_ACTIONS],
            bandit_scores: [0.0; N_ACTIONS],
            updates: 0,
            learned_mean_reward: 0.0,
            static_mean_reward: 0.0,
            reward_lift: 0.0,
            shadow_wins: 0,
            shadow_ties: 0,
            shadow_losses: 0,
            shadow_win_rate: 0.0,
            action_counts: [0; N_ACTIONS],
            wasted_boost_mins: 0,
            thermal_spike_mins: 0,
            memory_pressure_mins: 0,
        }
    }
}

impl LearnerSnapshot {
    pub fn to_json(&self) -> Json {
        let mut j = Json::obj();
        j.set("model_source", Json::str(self.model_source));
        j.set("architecture", Json::str(self.architecture.clone()));
        j.set("pred_cpu_pct", Json::num(self.pred_cpu_pct));
        j.set("burst_prob", Json::num(self.burst_prob));
        j.set("pred_t1_c", Json::num(self.pred_t1_c));
        j.set("pred_t3_c", Json::num(self.pred_t3_c));
        j.set("overheat_3m_prob", Json::num(self.overheat_3m_prob));
        j.set("th_burst", Json::num(self.th_burst));
        j.set("th_overheat", Json::num(self.th_overheat));
        j.set(
            "recommended_action",
            Json::str(self.recommended_action.as_str()),
        );
        j.set("static_action", Json::str(self.static_action.as_str()));
        j.set("decision_source", Json::str(self.decision_source));
        j.set("uclamp_min", Json::num(self.uclamp_min as f64));
        j.set("uclamp_max", Json::num(self.uclamp_max as f64));
        j.set(
            "action_probs",
            Json::Arr(self.action_probs.iter().map(|v| Json::num(*v)).collect()),
        );
        j.set(
            "fqi_q",
            Json::Arr(self.fqi_q.iter().map(|v| Json::num(*v)).collect()),
        );
        j.set(
            "bandit_scores",
            Json::Arr(self.bandit_scores.iter().map(|v| Json::num(*v)).collect()),
        );

        let mut shadow = Json::obj();
        shadow.set("updates", Json::num(self.updates as f64));
        shadow.set("learned_mean_reward", Json::num(self.learned_mean_reward));
        shadow.set("static_mean_reward", Json::num(self.static_mean_reward));
        shadow.set("reward_lift", Json::num(self.reward_lift));
        shadow.set("shadow_wins", Json::num(self.shadow_wins as f64));
        shadow.set("shadow_ties", Json::num(self.shadow_ties as f64));
        shadow.set("shadow_losses", Json::num(self.shadow_losses as f64));
        shadow.set("shadow_win_rate", Json::num(self.shadow_win_rate));
        let mut counts = Json::obj();
        for (i, &c) in self.action_counts.iter().enumerate() {
            counts.set(SchedAction::from_index(i).as_str(), Json::num(c as f64));
        }
        shadow.set("action_counts", counts);
        shadow.set(
            "wasted_boost_mins",
            Json::num(self.wasted_boost_mins as f64),
        );
        shadow.set(
            "thermal_spike_mins",
            Json::num(self.thermal_spike_mins as f64),
        );
        shadow.set(
            "memory_pressure_mins",
            Json::num(self.memory_pressure_mins as f64),
        );
        j.set("shadow", shadow);
        j
    }

    /// Privacy-safe summary for `/api/diagnostics`: explicitly excludes app names or raw profiles.
    pub fn to_diagnostics_json(&self) -> Json {
        let mut j = Json::obj();
        j.set("model_source", Json::str(self.model_source));
        j.set("architecture", Json::str(self.architecture.clone()));
        j.set(
            "recommended_action",
            Json::str(self.recommended_action.as_str()),
        );
        j.set("static_action", Json::str(self.static_action.as_str()));
        j.set("decision_source", Json::str(self.decision_source));
        j.set("updates", Json::num(self.updates as f64));
        j.set("reward_lift", Json::num(self.reward_lift));
        j.set("shadow_win_rate", Json::num(self.shadow_win_rate));
        j.set(
            "wasted_boost_mins",
            Json::num(self.wasted_boost_mins as f64),
        );
        j.set(
            "thermal_spike_mins",
            Json::num(self.thermal_spike_mins as f64),
        );
        j.set(
            "memory_pressure_mins",
            Json::num(self.memory_pressure_mins as f64),
        );
        j
    }
}

pub struct Learner {
    model: CompactGbdtModel,
    model_source: ModelSource,
    extractor: FeatureExtractor,
    bandit: ResidualLinUcb,
    shadow: ShadowEvaluator,
    last_minute_ms: u64,
    has_minute_sample: bool,
    dirty: bool,
}

impl Learner {
    pub fn embedded() -> Self {
        Self {
            model: CompactGbdtModel::embedded(),
            model_source: ModelSource::Embedded,
            extractor: FeatureExtractor::default(),
            bandit: ResidualLinUcb::default(),
            shadow: ShadowEvaluator::default(),
            last_minute_ms: 0,
            has_minute_sample: false,
            dirty: false,
        }
    }

    pub fn load(model_path: &Path, state_path: &Path) -> Self {
        let (model, model_source) = CompactGbdtModel::load_or_embedded(model_path);
        let (bandit, shadow) = Self::load_state_file(state_path);
        Self {
            model,
            model_source,
            extractor: FeatureExtractor::default(),
            bandit,
            shadow,
            last_minute_ms: 0,
            has_minute_sample: false,
            dirty: false,
        }
    }

    fn load_state_file(path: &Path) -> (ResidualLinUcb, ShadowEvaluator) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return (ResidualLinUcb::default(), ShadowEvaluator::default());
        };
        let Ok(j) = json::parse(&text) else {
            return (ResidualLinUcb::default(), ShadowEvaluator::default());
        };
        if j.get("schema").and_then(Json::as_u64) != Some(STATE_SCHEMA) {
            return (ResidualLinUcb::default(), ShadowEvaluator::default());
        }
        let bandit = j
            .get("bandit")
            .and_then(ResidualLinUcb::from_json)
            .unwrap_or_default();
        let shadow = j
            .get("shadow")
            .map(ShadowEvaluator::from_json)
            .unwrap_or_default();
        (bandit, shadow)
    }

    pub fn reload_model(&mut self, model_path: &Path) {
        let (model, source) = CompactGbdtModel::load_or_embedded(model_path);
        self.model = model;
        self.model_source = source;
    }

    pub fn reset(&mut self) {
        self.extractor.reset();
        self.bandit.reset();
        self.shadow.reset();
        self.has_minute_sample = false;
        self.last_minute_ms = 0;
        self.dirty = false;
    }

    pub fn usage_ranking(&self, limit: usize) -> Vec<(String, u32)> {
        self.extractor.usage_ranking(limit)
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_saved(&mut self) {
        self.dirty = false;
    }

    /// Serialize continuous-learning state with full IEEE-754 precision and zero package names.
    pub fn state_to_json_string(&self) -> String {
        let mut out = format!("{{\"schema\":{STATE_SCHEMA},\"bandit\":");
        self.bandit.write_exact_json(&mut out);
        out.push_str(",\"shadow\":");
        self.shadow.write_exact_json(&mut out);
        out.push('}');
        out
    }

    pub fn save_state(&mut self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, self.state_to_json_string())?;
        secure_file(&tmp);
        std::fs::rename(&tmp, path)?;
        self.dirty = false;
        Ok(())
    }

    pub fn on_tick(
        &mut self,
        now_ms: u64,
        wall_ms: u64,
        snap: &Snapshot,
        is_launcher: bool,
    ) -> LearnerSnapshot {
        let (raw_features, ctx) =
            self.extractor
                .extract(wall_ms, snap, is_launcher, &self.model.priors);
        let forecast = self.model.infer(&raw_features, &ctx);

        let prime_mhz = snap
            .cpu
            .clusters
            .iter()
            .max_by_key(|c| c.cpuinfo_max_khz)
            .map(|c| c.cur_khz as f64 / 1000.0)
            .unwrap_or(0.0);
        let temp_c = snap
            .thermal
            .cpu_max_c
            .or(snap.thermal.cpu_c)
            .unwrap_or(40.0);
        let psi_full10 = snap.psi.memory.full10;
        let mem_avail_pct = snap.mem.available_pct().clamp(0.0, 100.0);
        let batt_pct = snap.battery.capacity_pct.unwrap_or(70.0);

        let guard = PolicyGuardInput {
            temp_c,
            psi_full10,
            mem_avail_pct,
            overheat_prob: forecast.overheat_3m_prob,
            th_overheat: self.model.th_overheat,
        };
        let (recommended_action, bandit_scores, decision_source) =
            self.bandit.select(&ctx, &forecast.fqi_q, guard);

        let static_action = bandit::static_rule_action(StaticRuleInput {
            cpu_pct: snap.cpu.total_pct,
            prime_mhz,
            temp_c,
            psi_full10,
            mem_avail_pct,
            is_launcher,
            is_charging: snap.battery.charging,
            batt_pct,
        });

        let due_minute = !self.has_minute_sample
            || now_ms < self.last_minute_ms
            || now_ms.saturating_sub(self.last_minute_ms) >= MINUTE_CADENCE_MS;
        if due_minute {
            self.shadow.on_minute_step(
                &mut self.bandit,
                ctx,
                recommended_action,
                static_action,
                snap.cpu.total_pct,
                prime_mhz,
                temp_c,
                psi_full10,
                mem_avail_pct,
                is_launcher,
                snap.battery.charging,
                batt_pct,
            );
            self.extractor.push_minute_sample(snap);
            self.last_minute_ms = now_ms;
            self.has_minute_sample = true;
            self.dirty = true;
        }

        let (uclamp_min, uclamp_max) = recommended_action.uclamp();
        LearnerSnapshot {
            model_source: self.model_source.as_str(),
            architecture: self.model.architecture.clone(),
            pred_cpu_pct: forecast.pred_cpu_pct,
            burst_prob: forecast.burst_prob,
            pred_t1_c: forecast.pred_t1_c,
            pred_t3_c: forecast.pred_t3_c,
            overheat_3m_prob: forecast.overheat_3m_prob,
            th_burst: self.model.th_burst,
            th_overheat: self.model.th_overheat,
            recommended_action,
            static_action,
            decision_source,
            uclamp_min,
            uclamp_max,
            action_probs: forecast.action_probs,
            fqi_q: forecast.fqi_q,
            bandit_scores,
            updates: self.shadow.updates,
            learned_mean_reward: self.shadow.learned_mean_reward(),
            static_mean_reward: self.shadow.static_mean_reward(),
            reward_lift: self.shadow.reward_lift(),
            shadow_wins: self.shadow.shadow_wins,
            shadow_ties: self.shadow.shadow_ties,
            shadow_losses: self.shadow.shadow_losses,
            shadow_win_rate: self.shadow.shadow_win_rate(),
            action_counts: self.shadow.action_counts,
            wasted_boost_mins: self.shadow.wasted_boost_mins,
            thermal_spike_mins: self.shadow.thermal_spike_mins,
            memory_pressure_mins: self.shadow.memory_pressure_mins,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::gbdt::{CONTEXT_DIM, N_FEATURES};
    use super::*;
    use crate::types::ClusterInfo;

    const DEFAULT_MODEL_RAW: &str = include_str!("default_model.json");

    fn approx_eq(a: f64, b: f64, tol: f64, label: &str) {
        let diff = (a - b).abs();
        assert!(
            diff < tol,
            "{label}: rust={a} python={b} diff={diff} >= tol={tol}"
        );
    }

    fn make_snap(
        cpu_pct: f64,
        prime_khz: u32,
        temp_c: f64,
        fg: &str,
        psi_full: f64,
        mem_avail_kb: f64,
    ) -> Snapshot {
        let mut snap = Snapshot::default();
        snap.cpu.total_pct = cpu_pct;
        snap.cpu.clusters.push(ClusterInfo {
            name: "prime".into(),
            cpus: vec![6, 7],
            governor: "walt".into(),
            available_governors: vec!["walt".into()],
            cur_khz: prime_khz,
            min_khz: 1_017_600,
            max_khz: 4_320_000,
            cpuinfo_max_khz: 4_320_000,
            capacity: Some(1024),
            load_pct: cpu_pct,
            policy_path: "/sys/devices/system/cpu/cpufreq/policy6".into(),
        });
        snap.thermal.cpu_max_c = Some(temp_c);
        snap.thermal.cpu_c = Some(temp_c);
        snap.battery.temp_c = Some(34.0);
        snap.battery.capacity_pct = Some(72.0);
        snap.mem.total_kb = 15_113.4 * 1024.0;
        snap.mem.available_kb = mem_avail_kb;
        snap.psi.memory.full10 = psi_full;
        snap.psi.memory.some10 = psi_full * 1.2;
        snap.apps.foreground = Some(fg.into());
        snap
    }

    #[test]
    fn parity_matches_python_lightgbm_and_fqi() {
        let model = CompactGbdtModel::embedded();
        let j = json::parse(DEFAULT_MODEL_RAW).expect("valid default_model.json");
        let samples = j
            .get("parity_samples")
            .expect("parity_samples missing")
            .as_arr();
        assert_eq!(samples.len(), 3);

        for (idx, s) in samples.iter().enumerate() {
            let raw_vec: Vec<f64> = s
                .get("raw_features")
                .unwrap()
                .as_arr()
                .iter()
                .map(|x| x.as_f64().unwrap())
                .collect();
            let ctx_vec: Vec<f64> = s
                .get("context_15")
                .unwrap()
                .as_arr()
                .iter()
                .map(|x| x.as_f64().unwrap())
                .collect();
            let raw: [f64; N_FEATURES] = raw_vec.try_into().unwrap();
            let ctx: [f64; CONTEXT_DIM] = ctx_vec.try_into().unwrap();

            let pred = model.infer(&raw, &ctx);
            approx_eq(
                pred.pred_cpu_pct,
                s.get("expected_cpu").unwrap().as_f64().unwrap(),
                1e-3,
                &format!("sample[{idx}].cpu"),
            );
            approx_eq(
                pred.burst_prob,
                s.get("expected_burst_prob").unwrap().as_f64().unwrap(),
                1e-3,
                &format!("sample[{idx}].burst"),
            );
            approx_eq(
                pred.pred_t1_c,
                s.get("expected_t1").unwrap().as_f64().unwrap(),
                1e-3,
                &format!("sample[{idx}].t1"),
            );
            approx_eq(
                pred.pred_t3_c,
                s.get("expected_t3").unwrap().as_f64().unwrap(),
                1e-3,
                &format!("sample[{idx}].t3"),
            );
            approx_eq(
                pred.overheat_3m_prob,
                s.get("expected_oh_prob").unwrap().as_f64().unwrap(),
                1e-3,
                &format!("sample[{idx}].oh"),
            );

            let exp_probs = s.get("expected_action_probs").unwrap().as_arr();
            for (c, v) in exp_probs.iter().enumerate() {
                approx_eq(
                    pred.action_probs[c],
                    v.as_f64().unwrap(),
                    1e-3,
                    &format!("sample[{idx}].act[{c}]"),
                );
            }

            let exp_fqi = s.get("expected_fqi_q").unwrap().as_arr();
            for (a, v) in exp_fqi.iter().enumerate() {
                approx_eq(
                    pred.fqi_q[a],
                    v.as_f64().unwrap(),
                    1e-3,
                    &format!("sample[{idx}].fqi[{a}]"),
                );
            }
        }
    }

    #[test]
    fn corrupt_or_missing_model_falls_back_to_embedded() {
        let dir = std::env::temp_dir().join("neurosched-learner-fallback");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let missing = dir.join("missing.json");
        let (_, src_missing) = CompactGbdtModel::load_or_embedded(&missing);
        assert_eq!(src_missing, ModelSource::Embedded);

        let broken = dir.join("broken.json");
        std::fs::write(&broken, "{not valid json").unwrap();
        let (_, src_broken) = CompactGbdtModel::load_or_embedded(&broken);
        assert_eq!(src_broken, ModelSource::Embedded);

        // Cyclic tree must be rejected and safely fall back to embedded weights.
        let cyclic = json::parse(DEFAULT_MODEL_RAW).unwrap();
        let cyclic_str = cyclic.to_string().replacen("\"l\":[1,", "\"l\":[0,", 1);
        let cyclic_path = dir.join("cyclic.json");
        std::fs::write(&cyclic_path, cyclic_str).unwrap();
        let (_, src_cyclic) = CompactGbdtModel::load_or_embedded(&cyclic_path);
        assert_eq!(src_cyclic, ModelSource::Embedded);
        let _ = cyclic.get("schema");
    }

    #[test]
    fn sherman_morrison_matches_direct_inverse_and_adapts() {
        let mut bandit = ResidualLinUcb::default();
        // Track A_true directly (without forgetting) OR test with tiny noise updates:
        // verify online Sherman-Morrison update shifts learned mean reward positively toward BOOST.
        let mut ctx = [0.0f64; CONTEXT_DIM];
        ctx[0] = 0.65;
        ctx[2] = 0.44;
        ctx[14] = 1.0;

        // First verify single-action Sherman-Morrison rank-1 exact inverse identity against reconstructed A.
        let a_inv_0 = bandit.a_inv[3];
        let mut a_mat = [[0.0f64; CONTEXT_DIM]; CONTEXT_DIM];
        for (i, row) in a_mat.iter_mut().enumerate() {
            row[i] = bandit::INIT_LAMBDA;
        }
        // Check initial product A * A_inv == I.
        for (i, row) in a_mat.iter().enumerate() {
            for j in 0..CONTEXT_DIM {
                let s: f64 = row
                    .iter()
                    .zip(a_inv_0.iter())
                    .map(|(a, b_row)| a * b_row[j])
                    .sum();
                let target = if i == j { 1.0 } else { 0.0 };
                assert!((s - target).abs() < 1e-9);
            }
        }

        // Feed 50 synthetic updates where BOOST gets +1.0 reward and EFFICIENCY gets -0.5.
        for step in 0..50 {
            let mut x = [0.0f64; CONTEXT_DIM];
            x[0] = 0.4 + 0.01 * (step % 10) as f64;
            x[1] = 0.5;
            x[2] = 0.42;
            x[14] = 1.0;
            bandit.update(&x, SchedAction::Boost, 1.0);
            bandit.update(&x, SchedAction::Efficiency, -0.5);
        }

        let zero_fqi = [0.0f64; N_ACTIONS];
        let guard = PolicyGuardInput {
            temp_c: 44.0,
            psi_full10: 0.0,
            mem_avail_pct: 30.0,
            overheat_prob: 0.05,
            th_overheat: 0.30,
        };
        let (act, scores, source) = bandit.select(&ctx, &zero_fqi, guard);
        assert_eq!(source, "fqi_linucb");
        assert_eq!(act, SchedAction::Boost);
        assert!(scores[SchedAction::Boost as usize] > scores[SchedAction::Efficiency as usize]);
        assert!(scores[SchedAction::Boost as usize] > 0.2);

        // Verify exact Sherman-Morrison matrix positive-definiteness & symmetry on updated arm.
        for i in 0..CONTEXT_DIM {
            assert!(bandit.a_inv[SchedAction::Boost as usize][i][i] > 0.0);
            for j in 0..CONTEXT_DIM {
                let diff = (bandit.a_inv[SchedAction::Boost as usize][i][j]
                    - bandit.a_inv[SchedAction::Boost as usize][j][i])
                    .abs();
                assert!(diff < 1e-9);
            }
        }
    }

    #[test]
    fn shadow_evaluator_scores_and_tracks_win_rate_and_issues() {
        let mut learner = Learner::embedded();
        // Tick 0: launcher low CPU (6%) while Prime cluster is stuck boosting @ 2800 MHz -> wasted boost!
        let s0 = make_snap(6.0, 2_800_000, 46.0, "com.miui.home", 0.0, 5_000.0 * 1024.0);
        let d0 = learner.on_tick(1_000, 1_795_000_000_000, &s0, true);
        assert_eq!(d0.updates, 0);
        assert_eq!(d0.wasted_boost_mins, 1);

        // Tick 1 (60s later): hot 78C + high PSI full -> settles tick 0 and detects overheat/mem pressure.
        let s1 = make_snap(35.0, 3_200_000, 78.0, "app.game", 4.0, 900.0 * 1024.0);
        let d1 = learner.on_tick(62_000, 1_795_000_061_000, &s1, false);
        assert_eq!(d1.updates, 1);
        assert_eq!(d1.thermal_spike_mins, 1);
        assert_eq!(d1.memory_pressure_mins, 1);
        assert_eq!(d1.recommended_action, SchedAction::ThermalCap);
        assert_eq!(d1.decision_source, "safety_override");
    }

    #[test]
    fn learner_state_roundtrip_and_zero_package_leak() {
        let dir = std::env::temp_dir().join("neurosched-learner-persist");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state_path = dir.join("learner.json");

        let mut learner = Learner::embedded();
        let secret_pkg = "com.super.secret.banking";
        let s0 = make_snap(28.0, 2_900_000, 50.0, secret_pkg, 0.0, 4_500.0 * 1024.0);
        let s1 = make_snap(15.0, 1_400_000, 48.0, secret_pkg, 0.0, 4_500.0 * 1024.0);
        learner.on_tick(1_000, 1_795_000_000_000, &s0, false);
        let snap1 = learner.on_tick(65_000, 1_795_000_064_000, &s1, false);
        assert_eq!(snap1.updates, 1);
        assert!(learner.is_dirty());

        learner.save_state(&state_path).unwrap();
        assert!(!learner.is_dirty());

        let saved_raw = std::fs::read_to_string(&state_path).unwrap();
        let diag_raw = snap1.to_diagnostics_json().to_string();
        assert!(
            !saved_raw.contains(secret_pkg),
            "learner.json must not store package names"
        );
        assert!(
            !diag_raw.contains(secret_pkg),
            "learner diagnostics must not contain package names"
        );

        let mut reloaded = Learner::load(&dir.join("absent.json"), &state_path);
        let snap2 = reloaded.on_tick(1_000, 1_795_000_000_000, &s1, false);
        // Updates count before settling minute step 2 reflects persisted state (1).
        assert_eq!(snap2.updates, 1);
        assert!((snap2.learned_mean_reward - snap1.learned_mean_reward).abs() < 1e-9);
    }
}

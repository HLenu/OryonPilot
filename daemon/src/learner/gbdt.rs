//! Pure-Rust Compact GBDT + Offline FQI inference and 72-feature extractor.
//!
//! Zero external ML crates: parses exported JSON trees and preserves exact IEEE-754
//! `f64` split thresholds to match LightGBM runtime decisions.

use crate::json::{self, Json};
use crate::types::Snapshot;
use std::collections::{BTreeMap, VecDeque};
use std::f64::consts::TAU;
use std::path::Path;

pub const N_FEATURES: usize = 72;
pub const CONTEXT_DIM: usize = 15;
pub const N_ACTIONS: usize = 5;
const MAX_TREE_DEPTH: usize = 32;
const MAX_APP_PROFILES: usize = 256;
const HISTORY_MINUTES: usize = 16;

const EMBEDDED_MODEL_JSON: &str = include_str!("default_model.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    Embedded,
    HotLoaded,
}

impl ModelSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelSource::Embedded => "embedded",
            ModelSource::HotLoaded => "sched_model.json",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GlobalPriors {
    pub cpu_mean: f64,
    pub cpu_p90: f64,
    pub temp_mean: f64,
}

impl Default for GlobalPriors {
    fn default() -> Self {
        Self {
            cpu_mean: 15.0,
            cpu_p90: 30.0,
            temp_mean: 45.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompactTree {
    f: Vec<i16>,
    t: Vec<f64>,
    l: Vec<u16>,
    r: Vec<u16>,
    v: Vec<f64>,
}

impl CompactTree {
    fn from_json(j: &Json) -> Result<Self, String> {
        let f_arr = j.get("f").ok_or("missing tree.f")?.as_arr();
        let t_arr = j.get("t").ok_or("missing tree.t")?.as_arr();
        let l_arr = j.get("l").ok_or("missing tree.l")?.as_arr();
        let r_arr = j.get("r").ok_or("missing tree.r")?.as_arr();
        let v_arr = j.get("v").ok_or("missing tree.v")?.as_arr();

        let len = f_arr.len();
        if len == 0
            || t_arr.len() != len
            || l_arr.len() != len
            || r_arr.len() != len
            || v_arr.len() != len
            || len > u16::MAX as usize
        {
            return Err("invalid tree array dimensions".into());
        }

        let mut f = Vec::with_capacity(len);
        let mut t = Vec::with_capacity(len);
        let mut l = Vec::with_capacity(len);
        let mut r = Vec::with_capacity(len);
        let mut v = Vec::with_capacity(len);

        for i in 0..len {
            let feat = f_arr[i].as_i64().ok_or("invalid feature index")?;
            if feat >= N_FEATURES as i64 || feat < -1 {
                return Err("feature index out of bounds".into());
            }
            let thr = t_arr[i].as_f64().ok_or("non-finite threshold")?;
            let left = l_arr[i].as_u64().ok_or("invalid left child")?;
            let right = r_arr[i].as_u64().ok_or("invalid right child")?;
            let val = v_arr[i].as_f64().ok_or("non-finite leaf value")?;

            if feat >= 0 && (left as usize >= len || right as usize >= len) {
                return Err("child index out of bounds".into());
            }
            f.push(feat as i16);
            t.push(thr);
            l.push(left as u16);
            r.push(right as u16);
            v.push(val);
        }

        // Acyclic & depth guard so malformed trees cannot stall the 1 Hz control loop.
        let mut stack = vec![(0usize, 0usize)];
        while let Some((idx, depth)) = stack.pop() {
            if depth > MAX_TREE_DEPTH {
                return Err("tree exceeds maximum allowed depth".into());
            }
            if f[idx] >= 0 {
                stack.push((l[idx] as usize, depth + 1));
                stack.push((r[idx] as usize, depth + 1));
            }
        }

        Ok(Self { f, t, l, r, v })
    }

    #[inline]
    fn eval(&self, features: &[f64; N_FEATURES]) -> f64 {
        let mut idx = 0usize;
        for _ in 0..=MAX_TREE_DEPTH {
            let feat = self.f[idx];
            if feat < 0 {
                return self.v[idx];
            }
            idx = if features[feat as usize] <= self.t[idx] {
                self.l[idx] as usize
            } else {
                self.r[idx] as usize
            };
        }
        0.0
    }
}

#[derive(Debug, Clone)]
pub struct TreeEnsemble {
    trees: Vec<CompactTree>,
}

impl TreeEnsemble {
    fn from_json(j: &Json) -> Result<Self, String> {
        let arr = j.as_arr();
        if arr.is_empty() {
            return Err("empty tree ensemble".into());
        }
        let mut trees = Vec::with_capacity(arr.len());
        for node in arr {
            trees.push(CompactTree::from_json(node)?);
        }
        Ok(Self { trees })
    }

    #[inline]
    fn eval_sum(&self, features: &[f64; N_FEATURES]) -> f64 {
        self.trees.iter().map(|tr| tr.eval(features)).sum()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ModelForecast {
    pub pred_cpu_pct: f64,
    pub burst_prob: f64,
    pub pred_t1_c: f64,
    pub pred_t3_c: f64,
    pub overheat_3m_prob: f64,
    pub action_probs: [f64; N_ACTIONS],
    pub fqi_q: [f64; N_ACTIONS],
}

#[derive(Debug, Clone)]
pub struct CompactGbdtModel {
    pub schema: u32,
    pub architecture: String,
    pub priors: GlobalPriors,
    pub th_burst: f64,
    pub th_overheat: f64,
    cpu_head: TreeEnsemble,
    burst_head: TreeEnsemble,
    t1_head: TreeEnsemble,
    t3_head: TreeEnsemble,
    overheat_head: TreeEnsemble,
    action_heads: [TreeEnsemble; N_ACTIONS],
    fqi_weights: [[f64; CONTEXT_DIM]; N_ACTIONS],
}

impl CompactGbdtModel {
    pub fn embedded() -> Self {
        Self::parse_str(EMBEDDED_MODEL_JSON).expect("valid embedded default sched_model.json")
    }

    pub fn load_or_embedded(path: &Path) -> (Self, ModelSource) {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(model) = Self::parse_str(&text) {
                return (model, ModelSource::HotLoaded);
            }
        }
        (Self::embedded(), ModelSource::Embedded)
    }

    pub fn parse_str(raw: &str) -> Result<Self, String> {
        let j = json::parse(raw).map_err(|e| format!("json parse error: {e}"))?;
        let schema = j.get("schema").and_then(Json::as_u64).unwrap_or(0) as u32;
        if schema != 2 {
            return Err(format!("unsupported model schema {schema}"));
        }
        let feature_names = j
            .get("feature_names")
            .map(Json::as_arr)
            .ok_or("missing feature_names")?;
        if feature_names.len() != N_FEATURES {
            return Err(format!(
                "expected {N_FEATURES} features, got {}",
                feature_names.len()
            ));
        }

        let architecture = j
            .get("architecture")
            .and_then(Json::as_str)
            .unwrap_or("CompactGBDT_FQI")
            .to_string();

        let priors = GlobalPriors {
            cpu_mean: j
                .get_path("global_priors.cpu_mean")
                .and_then(Json::as_f64)
                .unwrap_or(15.0),
            cpu_p90: j
                .get_path("global_priors.cpu_p90")
                .and_then(Json::as_f64)
                .unwrap_or(30.0),
            temp_mean: j
                .get_path("global_priors.temp_mean")
                .and_then(Json::as_f64)
                .unwrap_or(45.0),
        };

        let th_burst = j
            .get("th_burst")
            .and_then(Json::as_f64)
            .unwrap_or(0.5)
            .clamp(0.01, 0.99);
        let th_overheat = j
            .get("th_overheat")
            .and_then(Json::as_f64)
            .unwrap_or(0.25)
            .clamp(0.01, 0.99);

        let heads = j.get("heads").ok_or("missing heads")?;
        let cpu_head = TreeEnsemble::from_json(heads.get("cpu").ok_or("missing cpu head")?)?;
        let burst_head = TreeEnsemble::from_json(heads.get("burst").ok_or("missing burst head")?)?;
        let t1_head = TreeEnsemble::from_json(heads.get("t1").ok_or("missing t1 head")?)?;
        let t3_head = TreeEnsemble::from_json(heads.get("t3").ok_or("missing t3 head")?)?;
        let overheat_head =
            TreeEnsemble::from_json(heads.get("overheat").ok_or("missing overheat head")?)?;

        let act_arr = heads.get("action").ok_or("missing action head")?.as_arr();
        if act_arr.len() != N_ACTIONS {
            return Err("expected 5 action class ensembles".into());
        }
        let action_heads = [
            TreeEnsemble::from_json(&act_arr[0])?,
            TreeEnsemble::from_json(&act_arr[1])?,
            TreeEnsemble::from_json(&act_arr[2])?,
            TreeEnsemble::from_json(&act_arr[3])?,
            TreeEnsemble::from_json(&act_arr[4])?,
        ];

        let fqi_arr = j
            .get_path("fqi.weights")
            .ok_or("missing fqi.weights")?
            .as_arr();
        if fqi_arr.len() != N_ACTIONS {
            return Err("expected 5 fqi weight rows".into());
        }
        let mut fqi_weights = [[0.0f64; CONTEXT_DIM]; N_ACTIONS];
        for (a, row_json) in fqi_arr.iter().enumerate() {
            let row = row_json.as_arr();
            if row.len() != CONTEXT_DIM {
                return Err("fqi weight row dimension mismatch".into());
            }
            for (c, val) in row.iter().enumerate() {
                fqi_weights[a][c] = val.as_f64().ok_or("non-finite fqi weight")?;
            }
        }

        Ok(Self {
            schema,
            architecture,
            priors,
            th_burst,
            th_overheat,
            cpu_head,
            burst_head,
            t1_head,
            t3_head,
            overheat_head,
            action_heads,
            fqi_weights,
        })
    }

    pub fn infer(
        &self,
        raw_features: &[f64; N_FEATURES],
        context_15: &[f64; CONTEXT_DIM],
    ) -> ModelForecast {
        let pred_cpu_pct = self.cpu_head.eval_sum(raw_features).clamp(0.0, 100.0);
        let burst_prob = sigmoid(self.burst_head.eval_sum(raw_features));
        let pred_t1_c = self.t1_head.eval_sum(raw_features);
        let pred_t3_c = self.t3_head.eval_sum(raw_features);
        let overheat_3m_prob = sigmoid(self.overheat_head.eval_sum(raw_features));

        let mut logits = [0.0f64; N_ACTIONS];
        for (c, head) in self.action_heads.iter().enumerate() {
            logits[c] = head.eval_sum(raw_features);
        }
        let action_probs = softmax5(logits);

        let mut fqi_q = [0.0f64; N_ACTIONS];
        for (a, w_row) in self.fqi_weights.iter().enumerate() {
            fqi_q[a] = dot15(w_row, context_15);
        }

        ModelForecast {
            pred_cpu_pct,
            burst_prob,
            pred_t1_c,
            pred_t3_c,
            overheat_3m_prob,
            action_probs,
            fqi_q,
        }
    }
}

#[derive(Debug, Clone)]
struct MinuteSample {
    cpu_pct: f64,
    prime_mhz_norm: f64,
    mem_avail_mb: f64,
    psi_some10: f64,
    cpu_temp_c: f64,
    batt_temp_c: f64,
    foreground: String,
}

impl MinuteSample {
    fn from_snapshot(snap: &Snapshot) -> Self {
        let prime_mhz = snap
            .cpu
            .clusters
            .iter()
            .max_by_key(|c| c.cpuinfo_max_khz)
            .map(|c| c.cur_khz as f64 / 1000.0)
            .unwrap_or(0.0);
        let cpu_temp_c = snap
            .thermal
            .cpu_max_c
            .or(snap.thermal.cpu_c)
            .unwrap_or(40.0);
        let batt_temp_c = snap.battery.temp_c.unwrap_or(30.0);
        let mem_avail_mb = snap.mem.available_kb / 1024.0;
        let foreground = snap
            .apps
            .foreground
            .clone()
            .unwrap_or_else(|| "-".to_string());
        Self {
            cpu_pct: snap.cpu.total_pct,
            prime_mhz_norm: prime_mhz / 1000.0,
            mem_avail_mb,
            psi_some10: snap.psi.memory.some10,
            cpu_temp_c,
            batt_temp_c,
            foreground,
        }
    }
}

#[derive(Debug, Clone)]
struct OnlineAppStats {
    count: u64,
    cpu_mean: f64,
    cpu_p90_proxy: f64,
    temp_mean: f64,
    dtemp_mean: f64,
    last_seen_seq: u64,
}

#[derive(Debug, Clone, Default)]
pub struct FeatureExtractor {
    history: VecDeque<MinuteSample>,
    app_stats: BTreeMap<String, OnlineAppStats>,
    seq: u64,
    fg_run_len: f64,
}

impl FeatureExtractor {
    pub fn reset(&mut self) {
        self.history.clear();
        self.app_stats.clear();
        self.seq = 0;
        self.fg_run_len = 0.0;
    }

    pub fn usage_ranking(&self, limit: usize) -> Vec<(String, u32)> {
        let mut list: Vec<(String, u32)> = self
            .app_stats
            .iter()
            .filter(|(k, _)| *k != "-")
            .map(|(k, st)| (k.clone(), st.count.min(u32::MAX as u64) as u32))
            .collect();
        list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        list.truncate(limit);
        list
    }

    pub fn push_minute_sample(&mut self, snap: &Snapshot) {
        let cur = MinuteSample::from_snapshot(snap);
        let prev_temp = self.history.back().map(|p| p.cpu_temp_c);
        let same_fg = self
            .history
            .back()
            .is_some_and(|p| p.foreground == cur.foreground);
        self.fg_run_len = if same_fg { self.fg_run_len + 1.0 } else { 1.0 };

        self.seq = self.seq.saturating_add(1);
        let seq = self.seq;
        let dtemp = prev_temp.map(|t| cur.cpu_temp_c - t).unwrap_or(0.0);
        let pkg = cur.foreground.clone();

        if self.app_stats.len() >= MAX_APP_PROFILES && !self.app_stats.contains_key(&pkg) {
            if let Some(evict) = self
                .app_stats
                .iter()
                .min_by_key(|(_, st)| st.last_seen_seq)
                .map(|(k, _)| k.clone())
            {
                self.app_stats.remove(&evict);
            }
        }

        let entry = self.app_stats.entry(pkg).or_insert(OnlineAppStats {
            count: 0,
            cpu_mean: cur.cpu_pct,
            cpu_p90_proxy: cur.cpu_pct,
            temp_mean: cur.cpu_temp_c,
            dtemp_mean: 0.0,
            last_seen_seq: seq,
        });
        entry.count = entry.count.saturating_add(1);
        let n = entry.count as f64;
        let alpha = (1.0 / n).max(0.05);
        entry.cpu_mean += (cur.cpu_pct - entry.cpu_mean) * alpha;
        // Online asymmetric quantile tracking targeting ~P90 CPU load.
        let q_step = if cur.cpu_pct >= entry.cpu_p90_proxy {
            0.18
        } else {
            -0.02
        };
        entry.cpu_p90_proxy = (entry.cpu_p90_proxy
            + q_step * (cur.cpu_pct - entry.cpu_p90_proxy).abs())
        .clamp(0.0, 100.0);
        entry.temp_mean += (cur.cpu_temp_c - entry.temp_mean) * alpha;
        entry.dtemp_mean += (dtemp - entry.dtemp_mean) * alpha;
        entry.last_seen_seq = seq;

        self.history.push_back(cur);
        while self.history.len() > HISTORY_MINUTES {
            self.history.pop_front();
        }
    }

    pub fn extract(
        &self,
        wall_ms: u64,
        snap: &Snapshot,
        is_launcher: bool,
        priors: &GlobalPriors,
    ) -> ([f64; N_FEATURES], [f64; CONTEXT_DIM]) {
        let cur = MinuteSample::from_snapshot(snap);

        // Local UTC+8 hour & day-of-week matching dataset.py (Jan 1, 1970 = Thursday = dow 3).
        let utc_secs = if wall_ms > 0 {
            wall_ms / 1000
        } else {
            snap.ts_ms / 1000
        };
        let local_secs = utc_secs + 8 * 3600;
        let hour_f = (local_secs % 86_400) as f64 / 3600.0;
        let hour_sin = (TAU * hour_f / 24.0).sin();
        let hour_cos = (TAU * hour_f / 24.0).cos();
        let dow = ((local_secs / 86_400 + 3) % 7) as f64;
        let dow_sin = (TAU * dow / 7.0).sin();
        let dow_cos = (TAU * dow / 7.0).cos();

        let mem_avail_pct = snap.mem.available_pct().clamp(0.0, 100.0);
        let swap_used_gb = (snap.mem.swap_used_kb() / 1024.0) / 1024.0;
        let zram_ratio = snap.mem.zram.first().map(|z| z.ratio).unwrap_or(0.0);
        let psi_full10 = snap.psi.memory.full10;
        let kswapd_pct = snap.mem.kswapd_cpu_pct;
        let temp_diff = cur.cpu_temp_c - cur.batt_temp_c;
        let batt_pct_norm = snap.battery.capacity_pct.unwrap_or(70.0) / 100.0;
        let is_charging = if snap.battery.charging { 1.0 } else { 0.0 };
        let is_launcher_f = if is_launcher { 1.0 } else { 0.0 };

        let lag = |k: usize| -> &MinuteSample {
            if self.history.is_empty() {
                &cur
            } else {
                let idx = self.history.len().saturating_sub(k);
                &self.history[idx]
            }
        };
        let lag1 = lag(1);
        let lag2 = lag(2);
        let lag3 = lag(3);
        let lag5 = lag(5);

        let d_cpu_1m = if self.history.is_empty() {
            0.0
        } else {
            cur.cpu_pct - lag1.cpu_pct
        };
        let d_temp_1m = if self.history.is_empty() {
            0.0
        } else {
            cur.cpu_temp_c - lag1.cpu_temp_c
        };
        let d_mem_1m = if self.history.is_empty() {
            0.0
        } else {
            cur.mem_avail_mb - lag1.mem_avail_mb
        };
        let d_prime_1m = if self.history.is_empty() {
            0.0
        } else {
            cur.prime_mhz_norm - lag1.prime_mhz_norm
        };
        let d_batt_temp_1m = if self.history.is_empty() {
            0.0
        } else {
            cur.batt_temp_c - lag1.batt_temp_c
        };
        let temp_slope_3m = (cur.cpu_temp_c - lag3.cpu_temp_c) / 3.0;
        let cpu_slope_3m = (cur.cpu_pct - lag3.cpu_pct) / 3.0;

        let fg_changed = if let Some(prev) = self.history.back() {
            if prev.foreground != cur.foreground {
                1.0
            } else {
                0.0
            }
        } else {
            0.0
        };
        let run_len = if let Some(prev) = self.history.back() {
            if prev.foreground == cur.foreground {
                self.fg_run_len + 1.0
            } else {
                1.0
            }
        } else {
            1.0
        };

        let (app_cpu_mean, app_cpu_p90, app_temp_mean, app_dtemp_mean, app_freq_log) =
            match self.app_stats.get(&cur.foreground) {
                Some(st) if st.count >= 3 => (
                    st.cpu_mean,
                    st.cpu_p90_proxy,
                    st.temp_mean,
                    st.dtemp_mean,
                    (st.count as f64).ln_1p(),
                ),
                _ => (priors.cpu_mean, priors.cpu_p90, priors.temp_mean, 0.0, 0.0),
            };

        let mut feat = [0.0f64; N_FEATURES];
        feat[0] = cur.cpu_pct;
        feat[1] = cur.prime_mhz_norm;
        feat[2] = mem_avail_pct;
        feat[3] = swap_used_gb;
        feat[4] = zram_ratio;
        feat[5] = cur.psi_some10;
        feat[6] = psi_full10;
        feat[7] = kswapd_pct;
        feat[8] = cur.cpu_temp_c;
        feat[9] = cur.batt_temp_c;
        feat[10] = temp_diff;
        feat[11] = batt_pct_norm;
        feat[12] = is_charging;
        feat[13] = is_launcher_f;
        feat[14] = hour_sin;
        feat[15] = hour_cos;
        feat[16] = dow_sin;
        feat[17] = dow_cos;
        feat[18] = d_cpu_1m;
        feat[19] = d_temp_1m;
        feat[20] = d_mem_1m;
        feat[21] = d_prime_1m;
        feat[22] = d_batt_temp_1m;
        feat[23] = temp_slope_3m;
        feat[24] = cpu_slope_3m;
        feat[25] = run_len;
        feat[26] = fg_changed;
        feat[27] = app_cpu_mean;
        feat[28] = app_cpu_p90;
        feat[29] = app_temp_mean;
        feat[30] = app_dtemp_mean;
        feat[31] = app_freq_log;

        // Rolling windows w in [3, 5, 10, 15] over [past..., cur] inclusive.
        let mut cursor = 32usize;
        let mut cpu_mean_5 = cur.cpu_pct;
        let mut temp_mean_5 = cur.cpu_temp_c;
        for &w in &[3usize, 5, 10, 15] {
            let take_hist = w.saturating_sub(1).min(self.history.len());
            let start = self.history.len() - take_hist;
            let count = (take_hist + 1) as f64;

            let mut cpu_sum = cur.cpu_pct;
            let mut cpu_max = cur.cpu_pct;
            let mut temp_sum = cur.cpu_temp_c;
            let mut temp_max = cur.cpu_temp_c;
            let mut prime_sum = cur.prime_mhz_norm;
            let mut psi_sum = cur.psi_some10;

            for item in self.history.iter().skip(start) {
                cpu_sum += item.cpu_pct;
                cpu_max = cpu_max.max(item.cpu_pct);
                temp_sum += item.cpu_temp_c;
                temp_max = temp_max.max(item.cpu_temp_c);
                prime_sum += item.prime_mhz_norm;
                psi_sum += item.psi_some10;
            }

            let c_mean = cpu_sum / count;
            let t_mean = temp_sum / count;
            let mut var_sum = (cur.cpu_pct - c_mean).powi(2);
            for item in self.history.iter().skip(start) {
                var_sum += (item.cpu_pct - c_mean).powi(2);
            }
            let c_std = if take_hist + 1 >= 2 {
                (var_sum / take_hist as f64).sqrt()
            } else {
                0.0
            };

            if w == 5 {
                cpu_mean_5 = c_mean;
                temp_mean_5 = t_mean;
            }

            feat[cursor] = c_mean;
            feat[cursor + 1] = cpu_max;
            feat[cursor + 2] = c_std;
            feat[cursor + 3] = t_mean;
            feat[cursor + 4] = temp_max;
            feat[cursor + 5] = prime_sum / count;
            feat[cursor + 6] = psi_sum / count;
            cursor += 7;
        }

        for item in &[lag1, lag2, lag3, lag5] {
            feat[cursor] = item.cpu_pct;
            feat[cursor + 1] = item.cpu_temp_c;
            feat[cursor + 2] = item.prime_mhz_norm;
            cursor += 3;
        }

        let ctx = [
            cur.cpu_pct / 100.0,
            cur.prime_mhz_norm / 4.5,
            cur.cpu_temp_c / 100.0,
            d_temp_1m / 10.0,
            d_cpu_1m / 50.0,
            mem_avail_pct / 100.0,
            cur.psi_some10 / 30.0,
            psi_full10 / 10.0,
            is_launcher_f,
            is_charging,
            batt_pct_norm,
            app_cpu_mean / 50.0,
            cpu_mean_5 / 50.0,
            temp_mean_5 / 100.0,
            1.0,
        ];

        (feat, ctx)
    }
}

#[inline]
fn sigmoid(z: f64) -> f64 {
    let clamped = z.clamp(-40.0, 40.0);
    1.0 / (1.0 + (-clamped).exp())
}

#[inline]
fn softmax5(logits: [f64; N_ACTIONS]) -> [f64; N_ACTIONS] {
    let max_v = logits.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mut exp = [0.0f64; N_ACTIONS];
    let mut sum = 0.0;
    for (i, &v) in logits.iter().enumerate() {
        let e = (v - max_v).exp();
        exp[i] = e;
        sum += e;
    }
    if sum > 0.0 {
        for slot in &mut exp {
            *slot /= sum;
        }
    }
    exp
}

#[inline]
pub fn dot15(a: &[f64; CONTEXT_DIM], b: &[f64; CONTEXT_DIM]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

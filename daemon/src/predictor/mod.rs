//! Predictive layer: Markov transitions + hour-of-week statistics + launch-interval
//! EWMA, fused into a single ranked guess with an explicit confidence.
//!
//! Contract:
//!  * Never launches anything. The output is advisory.
//!  * Refuses to answer when history is thin (see [`Prediction::refused`]).
//!  * Tracks its own hit rate so the UI can show whether it is worth keeping on.

pub mod confidence;
pub mod markov;
pub mod temporal;

use crate::config::PredictorConfig;
use crate::json::Json;
use crate::types::{AppEvent, AppEventKind};
use crate::util::Ewma;
use markov::MarkovChain;
use std::collections::{BTreeMap, BTreeSet};
use temporal::TemporalProfile;

/// How long after a prediction an actual foreground change still counts as a hit.
const HIT_HORIZON_MS: u64 = 120_000;
/// Cap tracked packages so long-running models remain bounded in memory and on disk.
const MAX_TRACKED_PACKAGES: usize = 256;

#[derive(Debug, Clone, Default)]
pub struct Prediction {
    pub package: Option<String>,
    pub probability: f64,
    pub confidence: f64,
    pub reason: String,
    pub sample_count: u64,
    pub refused: bool,
    pub candidates: Vec<(String, f64)>,
}

impl Prediction {
    pub fn refused(reason: impl Into<String>, samples: u64) -> Self {
        Prediction {
            reason: reason.into(),
            sample_count: samples,
            refused: true,
            ..Default::default()
        }
    }

    pub fn to_json(&self) -> Json {
        let mut j = Json::obj();
        j.set(
            "predicted_package",
            self.package.clone().map(Json::Str).unwrap_or(Json::Null),
        );
        j.set("probability", Json::num(self.probability));
        j.set("confidence", Json::num(self.confidence));
        j.set("reason", Json::str(self.reason.clone()));
        j.set("sample_count", Json::num(self.sample_count as f64));
        j.set("refused", Json::Bool(self.refused));
        j.set(
            "candidates",
            Json::Arr(
                self.candidates
                    .iter()
                    .map(|(p, s)| {
                        let mut o = Json::obj();
                        o.set("package", Json::str(p.clone()));
                        o.set("score", Json::num(*s));
                        o
                    })
                    .collect(),
            ),
        );
        j
    }
}

#[derive(Debug, Clone)]
struct Pending {
    package: String,
    ts: u64,
    source_trust: f64,
    top3: Vec<String>,
    baseline_most_frequent: Option<String>,
    baseline_previous: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub events: u64,
    pub opportunities: u64,
    pub predictions: u64,
    pub refused: u64,
    pub refused_reasons: BTreeMap<String, u64>,
    pub hits: u64,
    pub top3_hits: u64,
    /// Predictions settled by the next foreground change or by expiry.
    pub evaluated: u64,
    pub misses: u64,
    pub expired: u64,
    pub baseline_most_frequent_hits: u64,
    pub baseline_previous_hits: u64,
}

impl Stats {
    pub fn to_json(&self) -> Json {
        let mut reasons = Json::obj();
        for (code, count) in &self.refused_reasons {
            reasons.set(code, Json::num(*count as f64));
        }
        let mut j = Json::obj();
        j.set("events", Json::num(self.events as f64));
        j.set("opportunities", Json::num(self.opportunities as f64));
        j.set("predictions", Json::num(self.predictions as f64));
        j.set("refused", Json::num(self.refused as f64));
        j.set("refused_reasons", reasons);
        j.set("coverage", Json::num(self.coverage()));
        j.set("evaluated", Json::num(self.evaluated as f64));
        j.set("hits", Json::num(self.hits as f64));
        j.set("top3_hits", Json::num(self.top3_hits as f64));
        j.set("misses", Json::num(self.misses as f64));
        j.set("expired", Json::num(self.expired as f64));
        j.set("accuracy", Json::num(self.accuracy()));
        j.set("top3_accuracy", Json::num(self.top3_accuracy()));
        j.set("effective_accuracy", Json::num(self.effective_accuracy()));
        j.set(
            "baseline_most_frequent_hits",
            Json::num(self.baseline_most_frequent_hits as f64),
        );
        j.set(
            "baseline_most_frequent_accuracy",
            Json::num(self.baseline_most_frequent_accuracy()),
        );
        j.set(
            "baseline_previous_hits",
            Json::num(self.baseline_previous_hits as f64),
        );
        j.set(
            "baseline_previous_accuracy",
            Json::num(self.baseline_previous_accuracy()),
        );
        j
    }

    pub fn accuracy(&self) -> f64 {
        ratio(self.hits, self.evaluated)
    }

    pub fn top3_accuracy(&self) -> f64 {
        ratio(self.top3_hits, self.evaluated)
    }

    pub fn effective_accuracy(&self) -> f64 {
        ratio(self.hits, self.evaluated.saturating_sub(self.expired))
    }

    pub fn coverage(&self) -> f64 {
        ratio(self.predictions, self.opportunities)
    }

    pub fn baseline_most_frequent_accuracy(&self) -> f64 {
        ratio(self.baseline_most_frequent_hits, self.evaluated)
    }

    pub fn baseline_previous_accuracy(&self) -> f64 {
        ratio(self.baseline_previous_hits, self.evaluated)
    }
}

fn ratio(num: u64, den: u64) -> f64 {
    if den == 0 {
        0.0
    } else {
        num as f64 / den as f64
    }
}

pub struct Predictor {
    markov: MarkovChain,
    temporal: TemporalProfile,
    launches: BTreeMap<String, u32>,
    last_seen: BTreeMap<String, u64>,
    intervals: BTreeMap<String, Ewma>,
    ignored: BTreeSet<String>,
    previous: Option<String>,
    before_previous: Option<String>,
    pending: Option<Pending>,
    stats: Stats,
    last: Option<Prediction>,
    /// A display refresh must not create another forecast for the same transition.
    opportunity: Option<(u64, String)>,
    /// Tracks `(events, context, refused_code)` for the last opportunity counted in `stats`.
    last_opportunity_outcome: Option<(u64, String, Option<String>)>,
    last_event_ts: Option<u64>,
    dirty: bool,
    /// Historical polling-based counters are retained for inspection only.
    legacy_stats: Option<Json>,
}

impl Default for Predictor {
    fn default() -> Self {
        Self::new()
    }
}

impl Predictor {
    pub fn new() -> Self {
        Self {
            markov: MarkovChain::default(),
            temporal: TemporalProfile::default(),
            launches: BTreeMap::new(),
            last_seen: BTreeMap::new(),
            intervals: BTreeMap::new(),
            ignored: BTreeSet::new(),
            previous: None,
            before_previous: None,
            pending: None,
            stats: Stats::default(),
            last: None,
            opportunity: None,
            last_opportunity_outcome: None,
            last_event_ts: None,
            dirty: false,
            legacy_stats: None,
        }
    }

    pub fn set_ignored_packages(&mut self, packages: &[String]) {
        self.ignored = packages
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_saved(&mut self) {
        self.dirty = false;
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    pub fn last(&self) -> Option<&Prediction> {
        self.last.as_ref()
    }

    pub fn usage_ranking(&self, limit: usize) -> Vec<(String, u32)> {
        let mut v: Vec<(String, u32)> = self
            .launches
            .iter()
            .filter(|(k, _)| !self.ignored.contains(*k))
            .map(|(k, c)| (k.clone(), *c))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(limit);
        v
    }

    /// Feeds one event into the model and scores any outstanding prediction.
    pub fn observe(&mut self, ev: &AppEvent) {
        let pkg = ev.package.trim();
        if ev.kind != AppEventKind::Foreground || pkg.is_empty() {
            return;
        }
        if self.last_event_ts.is_some_and(|last| ev.ts_ms < last) {
            return;
        }
        self.last_event_ts = Some(ev.ts_ms);
        if self.ignored.contains(pkg) {
            return;
        }
        if self.previous.as_deref() == Some(pkg) {
            return;
        }
        if let Some(p) = self.pending.take() {
            let age = ev.ts_ms.checked_sub(p.ts);
            let in_horizon = age.is_some_and(|age| age <= HIT_HORIZON_MS);
            let expired = age.is_some_and(|age| age > HIT_HORIZON_MS);
            if in_horizon {
                self.settle_observation(&p, pkg);
            } else {
                self.settle_miss(expired);
            }
        }
        let prev_for_back = self.previous.take();
        self.markov.observe(prev_for_back.as_deref(), pkg);
        self.temporal.observe(ev.ts_ms, pkg);
        let count = self.launches.entry(pkg.to_string()).or_insert(0);
        *count = count.saturating_add(1);

        if let Some(prev_ts) = self.last_seen.get(pkg).copied() {
            let dt = ev.ts_ms.saturating_sub(prev_ts) as f64;
            if dt > 1_000.0 && dt < 86_400_000.0 {
                self.intervals
                    .entry(pkg.to_string())
                    .or_insert_with(|| Ewma::new(0.25))
                    .push(dt);
            }
        }
        self.last_seen.insert(pkg.to_string(), ev.ts_ms);
        self.before_previous = prev_for_back;
        self.previous = Some(pkg.to_string());
        self.stats.events = self.stats.events.saturating_add(1);
        self.dirty = true;
        self.prune_if_needed();
    }

    pub fn predict(
        &mut self,
        now_ms: u64,
        current: Option<&str>,
        cfg: &PredictorConfig,
        source_trust: f64,
    ) -> Prediction {
        let invalid_pending = self
            .pending
            .as_ref()
            .filter(|p| {
                !now_ms
                    .checked_sub(p.ts)
                    .is_some_and(|age| age <= HIT_HORIZON_MS)
            })
            .map(|pending| pending.ts);
        if let Some(ts) = invalid_pending {
            self.pending = None;
            let expired = now_ms
                .checked_sub(ts)
                .is_some_and(|age| age > HIT_HORIZON_MS);
            self.settle_miss(expired);
        }
        let source_trust = if source_trust.is_finite() {
            source_trust.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let raw_current = current.map(str::trim).filter(|p| !p.is_empty());
        let context = match raw_current {
            Some(pkg) if self.ignored.contains(pkg) => self.previous.as_deref(),
            other => other,
        };
        let opp_key = (self.stats.events, context.unwrap_or("").to_string());

        if !cfg.enabled {
            self.record_refusal(&opp_key, "disabled");
            return Prediction::refused("predictor disabled", self.stats.events);
        }
        if source_trust == 0.0 {
            self.record_refusal(&opp_key, "no_source");
            return Prediction::refused("no trusted event source", self.stats.events);
        }
        if self.stats.events < cfg.min_samples {
            self.record_refusal(&opp_key, "insufficient_history");
            return Prediction::refused(
                format!(
                    "insufficient history ({}/{})",
                    self.stats.events, cfg.min_samples
                ),
                self.stats.events,
            );
        }
        let Some(context) = context else {
            self.record_refusal(&opp_key, "no_foreground");
            return Prediction::refused("no foreground app yet", self.stats.events);
        };
        let opportunity = (self.stats.events, context.to_string());
        if self.opportunity.as_ref() == Some(&opportunity) {
            if let (Some(pending), Some(last)) = (&self.pending, &self.last) {
                let mut cached = last.clone();
                cached.confidence =
                    (last.confidence * source_trust / pending.source_trust).clamp(0.0, 1.0);
                cached.refused = cached.confidence < cfg.min_confidence;
                if cached.refused {
                    cached.reason =
                        "cached forecast refused after source trust or threshold changed".into();
                }
                return cached;
            }
            return Prediction::refused(
                "forecast for this foreground transition expired",
                self.stats.events,
            );
        }
        if self.pending.is_some() {
            return Prediction::refused("awaiting foreground event feedback", self.stats.events);
        }

        let markov = self.markov.distribution(context);
        let temporal_dist = self.temporal.distribution(now_ms);
        let temporal_sum: f64 = temporal_dist.values().sum();

        let mut keys: BTreeSet<String> = BTreeSet::new();
        keys.extend(markov.keys().cloned());
        keys.extend(temporal_dist.keys().cloned());
        keys.retain(|pkg| !self.ignored.contains(pkg));
        keys.remove(context);

        if keys.is_empty() {
            self.record_refusal(&opp_key, "no_candidates");
            return Prediction::refused("no transitions observed", self.stats.events);
        }

        let mut scored: Vec<(String, f64)> = keys
            .into_iter()
            .map(|pkg| {
                let m = markov.get(&pkg).copied().unwrap_or(0.0);
                let t = if temporal_sum > 0.0 {
                    temporal_dist.get(&pkg).copied().unwrap_or(0.0) / temporal_sum
                } else {
                    0.0
                };
                let recency = self.recency_score(now_ms, &pkg);
                (pkg, 0.50 * m + 0.35 * t + 0.15 * recency)
            })
            .collect();

        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        let total: f64 = scored.iter().map(|(_, s)| s).sum();
        if total <= 0.0 {
            self.record_refusal(&opp_key, "no_candidates");
            return Prediction::refused("all scores zero", self.stats.events);
        }
        for item in scored.iter_mut() {
            item.1 /= total;
        }

        let top1 = scored[0].1;
        let top2 = scored.get(1).map(|(_, s)| *s).unwrap_or(0.0);
        let probs: Vec<f64> = scored.iter().map(|(_, s)| *s).collect();
        let h = crate::util::entropy(&probs);

        let conf = confidence::score(&confidence::ConfidenceInput {
            samples: self.stats.events,
            min_samples: cfg.min_samples,
            top1,
            top2,
            entropy: h,
            candidates: scored.len(),
            source_trust,
        });

        let candidates: Vec<(String, f64)> =
            scored.iter().take(cfg.max_candidates).cloned().collect();

        let reason = format!(
            "markov={:.2} temporal={:.2} entropy={:.2} events={}",
            markov.get(&scored[0].0).copied().unwrap_or(0.0),
            if temporal_sum > 0.0 {
                temporal_dist.get(&scored[0].0).copied().unwrap_or(0.0) / temporal_sum
            } else {
                0.0
            },
            h,
            self.stats.events
        );

        if conf < cfg.min_confidence {
            self.record_refusal(&opp_key, "below_confidence");
            let p = Prediction {
                package: Some(scored[0].0.clone()),
                probability: top1,
                confidence: conf,
                reason: format!("below threshold {:.2}: {}", cfg.min_confidence, reason),
                sample_count: self.stats.events,
                refused: true,
                candidates,
            };
            self.last = Some(p.clone());
            return p;
        }

        // The next actual foreground transition settles this forecast. Repainting
        // the same forecast never extends its lifetime or increments the count.
        self.record_prediction(&opp_key);
        let ctx = opp_key.1.as_str();
        let top3 = candidates
            .iter()
            .take(3)
            .map(|(pkg, _)| pkg.clone())
            .collect();
        let baseline_most_frequent = self
            .launches
            .iter()
            .filter(|(pkg, _)| !self.ignored.contains(*pkg) && pkg.as_str() != ctx)
            .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
            .map(|(pkg, _)| pkg.clone());
        let baseline_previous = self
            .before_previous
            .as_ref()
            .filter(|pkg| !self.ignored.contains(*pkg) && pkg.as_str() != ctx)
            .cloned();
        self.opportunity = Some(opportunity);
        self.pending = Some(Pending {
            package: scored[0].0.clone(),
            ts: now_ms,
            source_trust,
            top3,
            baseline_most_frequent,
            baseline_previous,
        });
        let p = Prediction {
            package: Some(scored[0].0.clone()),
            probability: top1,
            confidence: conf,
            reason,
            sample_count: self.stats.events,
            refused: false,
            candidates,
        };
        self.last = Some(p.clone());
        p
    }

    fn record_refusal(&mut self, opp_key: &(u64, String), code: &str) {
        if let Some((events, ctx, _)) = &self.last_opportunity_outcome {
            if *events == opp_key.0 && ctx == &opp_key.1 {
                return;
            }
        }
        self.last_opportunity_outcome =
            Some((opp_key.0, opp_key.1.clone(), Some(code.to_string())));
        self.stats.opportunities = self.stats.opportunities.saturating_add(1);
        self.stats.refused = self.stats.refused.saturating_add(1);
        let entry = self
            .stats
            .refused_reasons
            .entry(code.to_string())
            .or_insert(0);
        *entry = entry.saturating_add(1);
        self.dirty = true;
    }

    fn record_prediction(&mut self, opp_key: &(u64, String)) {
        let mut upgraded = false;
        if let Some((events, ctx, old_code)) = &mut self.last_opportunity_outcome {
            if *events == opp_key.0 && ctx == &opp_key.1 {
                if let Some(code) = old_code.take() {
                    self.stats.refused = self.stats.refused.saturating_sub(1);
                    if let Some(cnt) = self.stats.refused_reasons.get_mut(&code) {
                        *cnt = cnt.saturating_sub(1);
                        if *cnt == 0 {
                            self.stats.refused_reasons.remove(&code);
                        }
                    }
                    upgraded = true;
                }
            }
        }
        if !upgraded {
            self.last_opportunity_outcome = Some((opp_key.0, opp_key.1.clone(), None));
            self.stats.opportunities = self.stats.opportunities.saturating_add(1);
        }
        self.stats.predictions = self.stats.predictions.saturating_add(1);
        self.dirty = true;
    }

    fn settle_observation(&mut self, p: &Pending, actual: &str) {
        self.stats.evaluated = self.stats.evaluated.saturating_add(1);
        if p.package == actual {
            self.stats.hits = self.stats.hits.saturating_add(1);
        } else {
            self.stats.misses = self.stats.misses.saturating_add(1);
        }
        if p.top3.iter().any(|c| c == actual) {
            self.stats.top3_hits = self.stats.top3_hits.saturating_add(1);
        }
        if p.baseline_most_frequent.as_deref() == Some(actual) {
            self.stats.baseline_most_frequent_hits =
                self.stats.baseline_most_frequent_hits.saturating_add(1);
        }
        if p.baseline_previous.as_deref() == Some(actual) {
            self.stats.baseline_previous_hits = self.stats.baseline_previous_hits.saturating_add(1);
        }
        self.dirty = true;
    }

    fn settle_miss(&mut self, expired: bool) {
        self.stats.evaluated = self.stats.evaluated.saturating_add(1);
        self.stats.misses = self.stats.misses.saturating_add(1);
        if expired {
            self.stats.expired = self.stats.expired.saturating_add(1);
        }
        self.dirty = true;
    }

    fn prune_if_needed(&mut self) {
        if self.launches.len() <= MAX_TRACKED_PACKAGES {
            return;
        }
        let mut ranked: Vec<(String, u32)> = self
            .launches
            .iter()
            .filter(|(pkg, _)| !self.ignored.contains(*pkg))
            .map(|(pkg, &cnt)| (pkg.clone(), cnt))
            .collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut keep: BTreeSet<String> = ranked
            .into_iter()
            .take(MAX_TRACKED_PACKAGES)
            .map(|(pkg, _)| pkg)
            .collect();
        for active in [&self.previous, &self.before_previous]
            .into_iter()
            .flatten()
        {
            if !self.ignored.contains(active) && keep.len() <= MAX_TRACKED_PACKAGES + 2 {
                keep.insert(active.clone());
            }
        }
        self.launches.retain(|pkg, _| keep.contains(pkg));
        self.last_seen.retain(|pkg, _| keep.contains(pkg));
        self.intervals.retain(|pkg, _| keep.contains(pkg));
        self.markov.retain_packages(&keep);
        self.temporal.retain_packages(&keep);
    }

    fn recency_score(&self, now_ms: u64, pkg: &str) -> f64 {
        let (Some(interval), Some(last)) = (self.intervals.get(pkg), self.last_seen.get(pkg))
        else {
            return 0.0;
        };
        let Some(avg) = interval.value() else {
            return 0.0;
        };
        if avg <= 0.0 {
            return 0.0;
        }
        let elapsed = now_ms.saturating_sub(*last) as f64;
        let ratio = (elapsed / avg).min(3.0);
        // Peak when the elapsed time is close to the learned interval.
        (1.0 - (ratio - 1.0).abs() / 2.0).clamp(0.0, 1.0)
    }

    pub fn to_json(&self) -> Json {
        let mut launches = Json::obj();
        for (k, v) in &self.launches {
            launches.set(k, Json::num(*v as f64));
        }
        let mut intervals = Json::obj();
        for (k, e) in &self.intervals {
            if let Some(v) = e.value() {
                intervals.set(k, Json::num(v));
            }
        }
        let mut refused_reasons = Json::obj();
        for (code, count) in &self.stats.refused_reasons {
            refused_reasons.set(code, Json::num(*count as f64));
        }
        let mut j = Json::obj();
        j.set("format_version", Json::num(2.0));
        j.set("stats_version", Json::num(3.0));
        j.set("markov", self.markov.to_json());
        j.set("temporal", self.temporal.to_json());
        j.set("launches", launches);
        j.set("intervals", intervals);
        j.set("events", Json::num(self.stats.events as f64));
        j.set("opportunities", Json::num(self.stats.opportunities as f64));
        j.set("predictions", Json::num(self.stats.predictions as f64));
        j.set("refused", Json::num(self.stats.refused as f64));
        j.set("refused_reasons", refused_reasons);
        j.set("hits", Json::num(self.stats.hits as f64));
        j.set("top3_hits", Json::num(self.stats.top3_hits as f64));
        j.set("evaluated", Json::num(self.stats.evaluated as f64));
        j.set("misses", Json::num(self.stats.misses as f64));
        j.set("expired", Json::num(self.stats.expired as f64));
        j.set(
            "baseline_most_frequent_hits",
            Json::num(self.stats.baseline_most_frequent_hits as f64),
        );
        j.set(
            "baseline_previous_hits",
            Json::num(self.stats.baseline_previous_hits as f64),
        );
        j.set(
            "legacy_stats",
            self.legacy_stats.clone().unwrap_or(Json::Null),
        );
        j
    }

    pub fn from_json(j: &Json) -> Self {
        let mut p = Predictor::new();
        if let Some(m) = j.get("markov") {
            p.markov = MarkovChain::from_json(m);
        }
        if let Some(t) = j.get("temporal") {
            p.temporal = TemporalProfile::from_json(t);
        }
        if let Some(Json::Obj(l)) = j.get("launches") {
            for (k, v) in l {
                if let Some(c) = v.as_u64() {
                    p.launches.insert(k.clone(), c.min(u32::MAX as u64) as u32);
                }
            }
        }
        if let Some(Json::Obj(iv)) = j.get("intervals") {
            for (k, v) in iv {
                if let Some(ms) = v.as_f64().filter(|ms| ms.is_finite() && *ms > 0.0) {
                    let mut e = Ewma::new(0.25);
                    e.push(ms);
                    p.intervals.insert(k.clone(), e);
                }
            }
        }
        p.stats.events = j.get("events").and_then(|v| v.as_u64()).unwrap_or(0);
        p.legacy_stats = j
            .get("legacy_stats")
            .filter(|v| matches!(v, Json::Obj(_)))
            .cloned();
        // Legacy prediction counts (v1 polling or v2 launcher-included) are
        // archived in `legacy_stats` while v3 baseline/coverage counters start fresh.
        match j.get("stats_version").and_then(|v| v.as_u64()) {
            Some(3) => {
                let evaluated = j.get("evaluated").and_then(|v| v.as_u64()).unwrap_or(0);
                p.stats.predictions = j.get("predictions").and_then(|v| v.as_u64()).unwrap_or(0);
                p.stats.refused = j.get("refused").and_then(|v| v.as_u64()).unwrap_or(0);
                p.stats.opportunities = j
                    .get("opportunities")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(p.stats.predictions.saturating_add(p.stats.refused))
                    .max(p.stats.predictions);
                if let Some(Json::Obj(reasons)) = j.get("refused_reasons") {
                    for (k, v) in reasons {
                        if let Some(c) = v.as_u64() {
                            p.stats.refused_reasons.insert(k.clone(), c);
                        }
                    }
                }
                p.stats.evaluated = evaluated.min(p.stats.predictions);
                p.stats.hits = j
                    .get("hits")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
                    .min(p.stats.evaluated);
                p.stats.top3_hits = j
                    .get("top3_hits")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
                    .min(p.stats.evaluated);
                p.stats.misses = p.stats.evaluated - p.stats.hits;
                p.stats.expired = j
                    .get("expired")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
                    .min(p.stats.misses);
                p.stats.baseline_most_frequent_hits = j
                    .get("baseline_most_frequent_hits")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
                    .min(p.stats.evaluated);
                p.stats.baseline_previous_hits = j
                    .get("baseline_previous_hits")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
                    .min(p.stats.evaluated);
            }
            other_version => {
                if p.legacy_stats.is_none() {
                    let mut legacy = Json::obj();
                    if let Some(ver) = other_version {
                        legacy.set("stats_version", Json::num(ver as f64));
                    }
                    for key in [
                        "events",
                        "predictions",
                        "evaluated",
                        "hits",
                        "misses",
                        "expired",
                    ] {
                        if let Some(value) = j.get(key) {
                            legacy.set(key, value.clone());
                        }
                    }
                    p.legacy_stats = Some(legacy);
                }
            }
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AppEventKind;

    fn ev(pkg: &str, ts: u64) -> AppEvent {
        AppEvent {
            ts_ms: ts,
            package: pkg.to_string(),
            kind: AppEventKind::Foreground,
        }
    }

    fn cfg() -> PredictorConfig {
        PredictorConfig {
            enabled: true,
            min_samples: 10,
            min_confidence: 0.30,
            max_candidates: 5,
            persist_interval_ms: 1_000,
        }
    }

    #[test]
    fn refuses_without_enough_history() {
        let mut p = Predictor::new();
        p.observe(&ev("com.a", 1_000));
        let pred = p.predict(2_000, Some("com.a"), &cfg(), 1.0);
        assert!(pred.refused);
        assert!(pred.reason.contains("insufficient"));
    }

    #[test]
    fn learns_a_stable_pattern_and_predicts_it() {
        let mut p = Predictor::new();
        // Repeat a -> b -> a -> c enough times to clear the sample gate.
        let mut ts = 0;
        for _ in 0..8 {
            for pkg in ["com.a", "com.b", "com.a", "com.c"] {
                ts += 60_000;
                p.observe(&ev(pkg, ts));
            }
        }
        let pred = p.predict(ts + 60_000, Some("com.a"), &cfg(), 1.0);
        assert!(!pred.refused, "reason={}", pred.reason);
        assert_eq!(pred.package.as_deref(), Some("com.b"));
        assert!(pred.confidence >= 0.30);
    }

    #[test]
    fn low_trust_source_is_refused() {
        let mut p = Predictor::new();
        let mut ts = 0;
        for _ in 0..8 {
            for pkg in ["com.a", "com.b"] {
                ts += 60_000;
                p.observe(&ev(pkg, ts));
            }
        }
        let pred = p.predict(ts + 1_000, Some("com.a"), &cfg(), 0.2);
        assert!(pred.refused);
    }

    #[test]
    fn hit_rate_is_tracked() {
        let mut p = Predictor::new();
        let mut ts = 0;
        for _ in 0..8 {
            for pkg in ["com.a", "com.b", "com.a", "com.c"] {
                ts += 60_000;
                p.observe(&ev(pkg, ts));
            }
        }
        let pred = p.predict(ts + 1_000, Some("com.a"), &cfg(), 1.0);
        assert!(!pred.refused);
        ts += 2_000;
        p.observe(&ev("com.b", ts));
        assert_eq!(p.stats().hits, 1);
        assert!(p.stats().accuracy() > 0.0);
    }

    #[test]
    fn persistence_roundtrip() {
        let mut p = Predictor::new();
        for i in 0..5 {
            p.observe(&ev("com.a", 1_000 + i));
        }
        let back = Predictor::from_json(&p.to_json());
        assert_eq!(back.stats().events, 1);
        assert_eq!(back.usage_ranking(1)[0].0, "com.a");
    }

    fn trained() -> (Predictor, u64) {
        let mut p = Predictor::new();
        let mut ts = 1_783_612_800_000;
        for _ in 0..8 {
            for pkg in ["com.b", "com.a"] {
                ts += 60_000;
                p.observe(&ev(pkg, ts));
            }
        }
        (p, ts)
    }

    #[test]
    fn refresh_does_not_replace_or_count_pending_forecast() {
        let (mut p, ts) = trained();
        let first = p.predict(ts + 1_000, Some("com.a"), &cfg(), 1.0);
        assert!(!first.refused);
        let refreshed = p.predict(ts + 100_000, Some("com.a"), &cfg(), 1.0);
        assert_eq!(first.package, refreshed.package);
        assert_eq!(p.stats().predictions, 1);
        p.observe(&ev("com.b", ts + 122_000));
        assert_eq!(p.stats().hits, 0);
        assert_eq!(p.stats().expired, 1);
        assert_eq!(p.stats().evaluated, 1);
    }

    #[test]
    fn first_wrong_foreground_settles_miss_without_late_hit() {
        let (mut p, ts) = trained();
        assert!(!p.predict(ts + 1_000, Some("com.a"), &cfg(), 1.0).refused);
        p.observe(&ev("com.c", ts + 2_000));
        p.observe(&ev("com.b", ts + 3_000));
        assert_eq!(p.stats().evaluated, 1);
        assert_eq!(p.stats().misses, 1);
        assert_eq!(p.stats().hits, 0);
    }

    #[test]
    fn screen_launch_and_duplicate_events_do_not_train_or_settle() {
        let (mut p, ts) = trained();
        assert!(!p.predict(ts + 1_000, Some("com.a"), &cfg(), 1.0).refused);
        let events = p.stats().events;
        p.observe(&ev("com.a", ts + 2_000));
        for kind in [
            AppEventKind::ScreenOn,
            AppEventKind::ScreenOff,
            AppEventKind::Launch,
        ] {
            let mut event = ev("com.b", ts + 3_000);
            event.kind = kind;
            p.observe(&event);
        }
        assert_eq!(p.stats().events, events);
        assert_eq!(p.stats().evaluated, 0);
        p.observe(&ev("com.b", ts + 4_000));
        assert_eq!(p.stats().hits, 1);
        assert_eq!(p.stats().accuracy(), 1.0);
    }

    #[test]
    fn future_forecast_time_cannot_be_scored_as_a_hit() {
        let (mut p, ts) = trained();
        assert!(!p.predict(ts + 10_000, Some("com.a"), &cfg(), 1.0).refused);
        p.observe(&ev("com.b", ts + 1_000));
        assert_eq!(p.stats().hits, 0);
        assert_eq!(p.stats().misses, 1);
    }

    #[test]
    fn expiry_is_evaluated_once_until_a_new_foreground_transition() {
        let (mut p, ts) = trained();
        assert!(!p.predict(ts + 1_000, Some("com.a"), &cfg(), 1.0).refused);
        assert!(p.predict(ts + 122_000, Some("com.a"), &cfg(), 1.0).refused);
        assert!(p.predict(ts + 123_000, Some("com.a"), &cfg(), 1.0).refused);
        assert_eq!(p.stats().predictions, 1);
        assert_eq!(p.stats().evaluated, 1);
        assert_eq!(p.stats().expired, 1);
    }

    #[test]
    fn cached_forecast_obeys_current_trust_and_threshold() {
        let (mut p, ts) = trained();
        assert!(!p.predict(ts + 1_000, Some("com.a"), &cfg(), 1.0).refused);
        assert!(p.predict(ts + 2_000, Some("com.a"), &cfg(), 0.1).refused);
        assert!(
            p.predict(ts + 3_000, Some("com.a"), &cfg(), f64::NAN)
                .refused
        );
        assert_eq!(p.stats().predictions, 1);
        assert_eq!(p.stats().evaluated, 0);
    }

    #[test]
    fn legacy_feedback_counts_are_not_reused_as_accuracy_evidence() {
        let (p, _) = trained();
        let mut json = p.to_json();
        if let Json::Obj(entries) = &mut json {
            entries.retain(|(key, _)| {
                ![
                    "evaluated",
                    "misses",
                    "expired",
                    "stats_version",
                    "format_version",
                    "legacy_stats",
                ]
                .contains(&key.as_str())
            });
        }
        json.set("predictions", Json::num(10_000.0));
        json.set("hits", Json::num(42.0));
        let restored = Predictor::from_json(&json);
        assert!(restored.stats().events > 0);
        assert_eq!(restored.stats().predictions, 0);
        assert_eq!(restored.stats().accuracy(), 0.0);
        let migrated = restored.to_json();
        assert_eq!(
            migrated.get("stats_version").and_then(|v| v.as_u64()),
            Some(3)
        );
        assert_eq!(
            migrated
                .get_path("legacy_stats.predictions")
                .and_then(|v| v.as_u64()),
            Some(10_000)
        );
        assert_eq!(
            migrated
                .get_path("legacy_stats.hits")
                .and_then(|v| v.as_u64()),
            Some(42)
        );
        let reloaded = Predictor::from_json(&migrated);
        assert_eq!(
            reloaded.to_json().get("legacy_stats"),
            migrated.get("legacy_stats")
        );
    }

    #[test]
    fn launchers_are_excluded_from_chain_ranking_and_candidates() {
        let mut p = Predictor::new();
        p.set_ignored_packages(&["com.miui.home".into()]);
        let mut ts = 1_783_612_800_000;
        for _ in 0..8 {
            for pkg in ["com.a", "com.miui.home", "com.b"] {
                ts += 10_000;
                p.observe(&ev(pkg, ts));
            }
        }
        assert_eq!(p.stats().events, 16);
        assert!(p
            .usage_ranking(10)
            .iter()
            .all(|(pkg, _)| pkg != "com.miui.home"));
        // Even while currently sitting on the launcher, context falls back to the last real app (com.b).
        let pred = p.predict(ts + 1_000, Some("com.miui.home"), &cfg(), 1.0);
        assert!(!pred.refused, "reason={}", pred.reason);
        assert_eq!(pred.package.as_deref(), Some("com.a"));
        assert!(pred
            .candidates
            .iter()
            .all(|(pkg, _)| pkg != "com.miui.home"));
        // Returning to launcher does not corrupt pending settlement; launching com.a settles hit.
        p.observe(&ev("com.miui.home", ts + 2_000));
        p.observe(&ev("com.a", ts + 3_000));
        assert_eq!(p.stats().hits, 1);
        assert_eq!(p.stats().evaluated, 1);
    }

    #[test]
    fn opportunities_refusals_and_baselines_are_counted_once_per_transition() {
        let mut p = Predictor::new();
        p.set_ignored_packages(&["com.miui.home".into()]);
        // Repeated polling under insufficient history counts one opportunity/refusal.
        p.observe(&ev("com.a", 1_000));
        for t in [2_000, 7_000, 12_000] {
            assert!(p.predict(t, Some("com.a"), &cfg(), 1.0).refused);
        }
        assert_eq!(p.stats().opportunities, 1);
        assert_eq!(p.stats().refused, 1);
        assert_eq!(
            p.stats().refused_reasons.get("insufficient_history"),
            Some(&1)
        );

        // Build pattern: com.c is most frequent overall, while com.a -> com.b -> com.a -> com.b...
        let mut ts = 20_000;
        for _ in 0..8 {
            for pkg in ["com.c", "com.d", "com.c"] {
                ts += 10_000;
                p.observe(&ev(pkg, ts));
            }
        }
        for _ in 0..4 {
            for pkg in ["com.a", "com.b"] {
                ts += 10_000;
                p.observe(&ev(pkg, ts));
            }
        }
        // Now at com.b (before_previous = com.a, most_frequent excluding com.b = com.c).
        let pred = p.predict(ts + 1_000, Some("com.b"), &cfg(), 1.0);
        assert!(!pred.refused);
        assert_eq!(pred.package.as_deref(), Some("com.a"));
        // Actual transition is com.a -> hits model Top-1, Top-3, and previous baseline, not most_frequent (com.c).
        p.observe(&ev("com.a", ts + 2_000));
        assert_eq!(p.stats().hits, 1);
        assert_eq!(p.stats().top3_hits, 1);
        assert_eq!(p.stats().baseline_previous_hits, 1);
        assert_eq!(p.stats().baseline_most_frequent_hits, 0);
        assert_eq!(p.stats().predictions, 1);
        assert_eq!(p.stats().opportunities, 2);
    }

    #[test]
    fn v2_stats_migrate_cleanly_to_v3_and_v3_roundtrips() {
        let (p, _) = trained();
        let mut v2 = p.to_json();
        v2.set("stats_version", Json::num(2.0));
        v2.set("predictions", Json::num(13_164.0));
        v2.set("evaluated", Json::num(13_159.0));
        v2.set("hits", Json::num(6_769.0));
        v2.set("misses", Json::num(6_390.0));
        v2.set("expired", Json::num(1_386.0));
        let mut migrated = Predictor::from_json(&v2);
        assert!(migrated.stats().events > 0);
        assert_eq!(migrated.stats().predictions, 0);
        assert_eq!(migrated.stats().hits, 0);
        let out = migrated.to_json();
        assert_eq!(out.get("stats_version").and_then(|v| v.as_u64()), Some(3));
        assert_eq!(
            out.get_path("legacy_stats.hits").and_then(|v| v.as_u64()),
            Some(6_769)
        );
        assert_eq!(
            out.get_path("legacy_stats.stats_version")
                .and_then(|v| v.as_u64()),
            Some(2)
        );

        migrated.stats.opportunities = 10;
        migrated.stats.predictions = 6;
        migrated.stats.refused = 4;
        migrated
            .stats
            .refused_reasons
            .insert("below_confidence".into(), 4);
        migrated.stats.evaluated = 6;
        migrated.stats.hits = 4;
        migrated.stats.top3_hits = 5;
        migrated.stats.misses = 2;
        migrated.stats.expired = 1;
        migrated.stats.baseline_most_frequent_hits = 2;
        migrated.stats.baseline_previous_hits = 3;
        let round = Predictor::from_json(&migrated.to_json());
        assert_eq!(round.stats().opportunities, 10);
        assert_eq!(round.stats().refused, 4);
        assert_eq!(round.stats().top3_hits, 5);
        assert_eq!(round.stats().baseline_most_frequent_hits, 2);
        assert_eq!(round.stats().baseline_previous_hits, 3);
        assert!((round.stats().effective_accuracy() - 0.8).abs() < 1e-9);
    }

    #[test]
    fn dirty_flag_and_package_cap_keep_model_bounded() {
        let mut p = Predictor::new();
        assert!(!p.is_dirty());
        let mut ts = 1_000_000;
        for i in 0..300 {
            ts += 5_000;
            p.observe(&ev(&format!("com.pkg.{i}"), ts));
        }
        assert!(p.is_dirty());
        assert!(p.launches.len() <= MAX_TRACKED_PACKAGES + 2);
        p.mark_saved();
        assert!(!p.is_dirty());
    }
}

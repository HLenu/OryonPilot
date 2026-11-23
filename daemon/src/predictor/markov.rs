//! First-order Markov chain over application transitions.
//!
//! Deliberately simple: a transition table is cheap to update, cheap to persist and
//! explainable in the UI ("com.a → com.b was seen 12 of 31 times"). No model runs
//! here, and nothing is ever launched based on a prediction.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct MarkovChain {
    /// from -> to -> count
    table: BTreeMap<String, BTreeMap<String, u32>>,
    total: u64,
}

impl MarkovChain {
    pub fn observe(&mut self, from: Option<&str>, to: &str) {
        let Some(from) = from else { return };
        if from == to {
            return;
        }
        let row = self.table.entry(from.to_string()).or_default();
        let count = row.entry(to.to_string()).or_insert(0);
        *count = count.saturating_add(1);
        self.total = self.total.saturating_add(1);
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn row_total(&self, from: &str) -> u32 {
        self.table
            .get(from)
            .map(|r| r.values().copied().fold(0, u32::saturating_add))
            .unwrap_or(0)
    }

    /// Normalised distribution over successors of `from`.
    pub fn distribution(&self, from: &str) -> BTreeMap<String, f64> {
        let mut out = BTreeMap::new();
        let Some(row) = self.table.get(from) else {
            return out;
        };
        let total: u64 = row.values().map(|count| *count as u64).sum();
        if total == 0 {
            return out;
        }
        for (k, v) in row {
            out.insert(k.clone(), *v as f64 / total as f64);
        }
        out
    }

    pub fn top_successors(&self, from: &str, limit: usize) -> Vec<(String, u32)> {
        let Some(row) = self.table.get(from) else {
            return Vec::new();
        };
        let mut v: Vec<(String, u32)> = row.iter().map(|(k, c)| (k.clone(), *c)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(limit);
        v
    }

    pub fn retain_packages(&mut self, keep: &std::collections::BTreeSet<String>) {
        self.table.retain(|from, row| {
            if !keep.contains(from) {
                return false;
            }
            row.retain(|to, _| keep.contains(to));
            !row.is_empty()
        });
        self.total = self
            .table
            .values()
            .flat_map(|row| row.values())
            .fold(0u64, |acc, &c| acc.saturating_add(c as u64));
    }

    pub fn to_json(&self) -> crate::json::Json {
        use crate::json::Json;
        let mut rows = Json::obj();
        for (from, row) in &self.table {
            let mut r = Json::obj();
            for (to, count) in row {
                r.set(to, Json::num(*count as f64));
            }
            rows.set(from, r);
        }
        let mut j = Json::obj();
        j.set("total", Json::num(self.total as f64));
        j.set("table", rows);
        j
    }

    #[allow(clippy::field_reassign_with_default)]
    pub fn from_json(j: &crate::json::Json) -> Self {
        let mut chain = MarkovChain::default();
        chain.total = j.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
        if let Some(Json::Obj(rows)) = j.get("table") {
            for (from, row) in rows {
                if let Json::Obj(pairs) = row {
                    let mut r = BTreeMap::new();
                    for (to, count) in pairs {
                        if let Some(c) = count.as_u64() {
                            r.insert(to.clone(), c.min(u32::MAX as u64) as u32);
                        }
                    }
                    chain.table.insert(from.clone(), r);
                }
            }
        }
        chain
    }
}

use crate::json::Json;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_counts_and_distribution() {
        let mut m = MarkovChain::default();
        m.observe(Some("com.a"), "com.b");
        m.observe(Some("com.a"), "com.b");
        m.observe(Some("com.a"), "com.c");
        assert_eq!(m.row_total("com.a"), 3);
        let d = m.distribution("com.a");
        assert!((d["com.b"] - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(m.top_successors("com.a", 1)[0].0, "com.b");
    }

    #[test]
    fn self_transition_is_ignored() {
        let mut m = MarkovChain::default();
        m.observe(Some("com.a"), "com.a");
        assert_eq!(m.total(), 0);
        assert!(m.distribution("com.a").is_empty());
    }

    #[test]
    fn roundtrip() {
        let mut m = MarkovChain::default();
        m.observe(Some("com.a"), "com.b");
        let back = MarkovChain::from_json(&m.to_json());
        assert_eq!(back.row_total("com.a"), 1);
        assert_eq!(back.total(), 1);
    }

    #[test]
    fn loaded_large_counts_cannot_wrap_or_overflow_distribution() {
        let mut json = MarkovChain::default().to_json();
        let mut row = Json::obj();
        row.set("com.b", Json::num(u64::MAX as f64));
        row.set("com.c", Json::num(u64::MAX as f64));
        let mut table = Json::obj();
        table.set("com.a", row);
        json.set("table", table);
        let mut chain = MarkovChain::from_json(&json);
        chain.observe(Some("com.a"), "com.b");
        assert_eq!(chain.row_total("com.a"), u32::MAX);
        let dist = chain.distribution("com.a");
        assert!((dist["com.b"] - 0.5).abs() < 1e-9);
        assert!((dist.values().sum::<f64>() - 1.0).abs() < 1e-9);
    }
}

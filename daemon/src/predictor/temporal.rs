//! Hour-of-week usage statistics.
//!
//! The bucket is derived from the epoch clock (UTC). Because the offset from local
//! time is constant on a given device, the *shape* of the distribution is preserved
//! without needing tzdata or a libc call.

use std::collections::BTreeMap;

pub const BUCKETS: usize = 7 * 24;

#[derive(Debug, Clone, Default)]
pub struct TemporalProfile {
    /// pkg -> bucket -> count
    table: BTreeMap<String, Vec<u32>>,
    bucket_totals: Vec<u32>,
    total: u64,
}

pub fn bucket_of(epoch_ms: u64) -> usize {
    ((epoch_ms / 3_600_000) % BUCKETS as u64) as usize
}

impl TemporalProfile {
    pub fn observe(&mut self, epoch_ms: u64, package: &str) {
        let b = bucket_of(epoch_ms);
        self.ensure();
        if let Some(row) = self.table.get_mut(package) {
            row[b] = row[b].saturating_add(1);
            self.bucket_totals[b] = self.bucket_totals[b].saturating_add(1);
            self.total = self.total.saturating_add(1);
            return;
        }
        let mut row = vec![0u32; BUCKETS];
        row[b] = 1;
        self.table.insert(package.to_string(), row);
        self.bucket_totals[b] = self.bucket_totals[b].saturating_add(1);
        self.total = self.total.saturating_add(1);
    }

    fn ensure(&mut self) {
        if self.bucket_totals.len() != BUCKETS {
            self.bucket_totals = vec![0; BUCKETS];
        }
        for row in self.table.values_mut() {
            if row.len() != BUCKETS {
                row.resize(BUCKETS, 0);
            }
        }
    }

    /// P(package | current bucket), Laplace-smoothed so an unseen app is not zero.
    pub fn probability(&self, epoch_ms: u64, package: &str) -> f64 {
        let b = bucket_of(epoch_ms);
        let total = self.bucket_totals.get(b).copied().unwrap_or(0) as f64;
        let count = self
            .table
            .get(package)
            .and_then(|r| r.get(b))
            .copied()
            .unwrap_or(0) as f64;
        if total <= 0.0 {
            return 0.0;
        }
        (count + 1.0) / (total + 2.0)
    }

    pub fn distribution(&self, epoch_ms: u64) -> BTreeMap<String, f64> {
        let b = bucket_of(epoch_ms);
        let total = self.bucket_totals.get(b).copied().unwrap_or(0) as f64;
        if total <= 0.0 {
            return BTreeMap::new();
        }
        let mut out = BTreeMap::new();
        for (pkg, row) in &self.table {
            let c = row.get(b).copied().unwrap_or(0) as f64;
            if c > 0.0 {
                out.insert(pkg.clone(), (c + 1.0) / (total + 2.0));
            }
        }
        out
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn retain_packages(&mut self, keep: &std::collections::BTreeSet<String>) {
        self.table.retain(|pkg, _| keep.contains(pkg));
        self.ensure();
        self.bucket_totals.fill(0);
        let mut total = 0u64;
        for row in self.table.values() {
            for (b, &count) in row.iter().enumerate() {
                self.bucket_totals[b] = self.bucket_totals[b].saturating_add(count);
                total = total.saturating_add(count as u64);
            }
        }
        self.total = total;
    }

    pub fn to_json(&self) -> crate::json::Json {
        use crate::json::Json;
        let mut table = Json::obj();
        for (pkg, row) in &self.table {
            table.set(
                pkg,
                Json::Arr(row.iter().map(|c| Json::num(*c as f64)).collect()),
            );
        }
        let mut j = Json::obj();
        j.set("total", Json::num(self.total as f64));
        j.set(
            "bucket_totals",
            Json::Arr(
                self.bucket_totals
                    .iter()
                    .map(|c| Json::num(*c as f64))
                    .collect(),
            ),
        );
        j.set("table", table);
        j
    }

    #[allow(clippy::field_reassign_with_default)]
    pub fn from_json(j: &crate::json::Json) -> Self {
        let mut p = TemporalProfile::default();
        p.total = j.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
        if let Some(arr) = j.get("bucket_totals") {
            p.bucket_totals = arr
                .as_arr()
                .iter()
                .filter_map(|v| v.as_u64().map(|c| c.min(u32::MAX as u64) as u32))
                .collect();
        }
        if let Some(Json::Obj(table)) = j.get("table") {
            for (pkg, row) in table {
                let vals: Vec<u32> = row
                    .as_arr()
                    .iter()
                    .filter_map(|v| v.as_u64().map(|c| c.min(u32::MAX as u64) as u32))
                    .collect();
                p.table.insert(pkg.clone(), vals);
            }
        }
        p.ensure();
        p
    }
}

use crate::json::Json;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_wraps_weekly() {
        let a = bucket_of(0);
        let b = bucket_of(7 * 24 * 3_600_000);
        assert_eq!(a, b);
        assert_eq!(bucket_of(3_600_000), 1);
    }

    #[test]
    fn probability_reflects_history() {
        let mut p = TemporalProfile::default();
        // Monday-ish 10:00 UTC
        let t = 10 * 3_600_000;
        for _ in 0..5 {
            p.observe(t, "com.a");
        }
        for _ in 0..5 {
            p.observe(t, "com.b");
        }
        let pa = p.probability(t, "com.a");
        let pb = p.probability(t, "com.b");
        assert!(pa > 0.0 && (pa - pb).abs() < 1e-9);
        // An empty bucket yields zero rather than a fabricated prior.
        assert_eq!(p.probability(100 * 3_600_000, "com.a"), 0.0);
    }

    #[test]
    fn roundtrip() {
        let mut p = TemporalProfile::default();
        p.observe(3_600_000, "com.a");
        let back = TemporalProfile::from_json(&p.to_json());
        assert_eq!(back.total(), 1);
        assert!(back.probability(3_600_000, "com.a") > 0.0);
    }
}

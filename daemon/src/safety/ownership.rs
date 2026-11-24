//! Node ownership bookkeeping.
//!
//! If Scene (or any other service) writes the same node we do, the daemon must stop
//! writing rather than fight it. Ownership is tracked per node with the original
//! value kept for restore.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    Ours,
    Unknown,
}

impl Owner {
    pub fn as_str(&self) -> &str {
        match self {
            Owner::Ours => "neurosched",
            Owner::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Ownership {
    owners: BTreeMap<String, Owner>,
    originals: BTreeMap<String, String>,
    last_written: BTreeMap<String, String>,
    released: BTreeSet<String>,
    notes: BTreeMap<String, String>,
}

impl Ownership {
    pub fn record_original(&mut self, node: &str, value: &str) {
        self.originals
            .entry(node.to_string())
            .or_insert_with(|| value.to_string());
        self.owners
            .entry(node.to_string())
            .or_insert(Owner::Unknown);
    }

    pub fn original(&self, node: &str) -> Option<&str> {
        self.originals.get(node).map(|s| s.as_str())
    }

    /// Capture the value immediately before our first write, after observation.
    pub fn record_before_write(&mut self, node: &str, value: &str) {
        if !self.last_written.contains_key(node) {
            self.originals.insert(node.to_string(), value.to_string());
        }
    }

    pub fn last_written(&self, node: &str) -> Option<&str> {
        self.last_written.get(node).map(|s| s.as_str())
    }

    pub fn owner(&self, node: &str) -> Owner {
        self.owners.get(node).cloned().unwrap_or(Owner::Unknown)
    }

    pub fn mark_ours(&mut self, node: &str, value: &str) {
        self.owners.insert(node.to_string(), Owner::Ours);
        self.last_written
            .insert(node.to_string(), value.to_string());
    }

    /// Give up a node for the rest of the session.
    pub fn release(&mut self, node: &str, why: impl Into<String>) {
        self.released.insert(node.to_string());
        self.notes.insert(node.to_string(), why.into());
    }

    pub fn is_released(&self, node: &str) -> bool {
        self.released.contains(node)
    }

    pub fn note(&self, node: &str) -> Option<&str> {
        self.notes.get(node).map(|s| s.as_str())
    }

    pub fn released_nodes(&self) -> Vec<String> {
        self.released.iter().cloned().collect()
    }

    /// True when the current value is still exactly what we last wrote. Only then is
    /// restoring the original safe: otherwise another service has taken over.
    pub fn still_ours(&self, node: &str, current: &str) -> bool {
        match self.last_written.get(node) {
            Some(v) => v == current,
            None => false,
        }
    }

    pub fn dump(&self) -> Vec<NodeView> {
        let mut nodes: BTreeSet<&String> = BTreeSet::new();
        nodes.extend(self.originals.keys());
        nodes.extend(self.last_written.keys());
        nodes
            .into_iter()
            .map(|node| NodeView {
                node: node.clone(),
                owner: self.owner(node).as_str().to_string(),
                original: self.original(node).map(|s| s.to_string()),
                last_written: self.last_written.get(node).cloned(),
                released: self.is_released(node),
                note: self.note(node).map(|s| s.to_string()),
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct NodeView {
    pub node: String,
    pub owner: String,
    pub original: Option<String>,
    pub last_written: Option<String>,
    pub released: bool,
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_is_kept_once() {
        let mut o = Ownership::default();
        o.record_original("/n", "100");
        o.record_original("/n", "200");
        assert_eq!(o.original("/n"), Some("100"));
    }

    #[test]
    fn still_ours_detects_external_write() {
        let mut o = Ownership::default();
        o.mark_ours("/n", "300");
        assert!(o.still_ours("/n", "300"));
        assert!(!o.still_ours("/n", "400"));
        assert!(!o.still_ours("/unknown", "300"));
    }

    #[test]
    fn release_blocks_further_writes() {
        let mut o = Ownership::default();
        o.release("/n", "scene owns this");
        assert!(o.is_released("/n"));
        assert_eq!(o.note("/n"), Some("scene owns this"));
        assert_eq!(o.released_nodes(), vec!["/n".to_string()]);
    }

    #[test]
    fn dump_includes_all_known_nodes() {
        let mut o = Ownership::default();
        o.record_original("/a", "1");
        o.mark_ours("/a", "2");
        o.mark_ours("/b", "3");
        let d = o.dump();
        assert_eq!(d.len(), 2);
        assert!(d
            .iter()
            .any(|n| n.node == "/a" && n.last_written.as_deref() == Some("2")));
    }
}

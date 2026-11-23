//! Safety layer: ownership, write journal, boot watchdog.

pub mod liveness;
pub mod lock;
pub mod ownership;
pub mod transaction;
pub mod watchdog;

pub use ownership::{NodeView, Owner, Ownership};
pub use transaction::{Journal, JournalEntry};
pub use watchdog::{BootState, Watchdog, WatchdogStatus};

use crate::json::Json;

/// Aggregated view for the Safety page.
#[derive(Debug, Clone, Default)]
pub struct SafetyView {
    pub watchdog: String,
    pub safe_mode: bool,
    pub safe_reason: String,
    pub writes_allowed: bool,
    pub mode: String,
    pub nodes: Vec<NodeView>,
    pub conflicts: Vec<String>,
    pub journal: Vec<JournalEntry>,
    pub recovered: Vec<String>,
}

impl SafetyView {
    pub fn to_json(&self) -> Json {
        let mut j = Json::obj();
        j.set("watchdog", Json::str(self.watchdog.clone()));
        j.set("safe_mode", Json::Bool(self.safe_mode));
        j.set("safe_reason", Json::str(self.safe_reason.clone()));
        j.set("writes_allowed", Json::Bool(self.writes_allowed));
        j.set("mode", Json::str(self.mode.clone()));
        j.set(
            "nodes",
            Json::Arr(
                self.nodes
                    .iter()
                    .map(|n| {
                        let mut o = Json::obj();
                        o.set("node", Json::str(n.node.clone()));
                        o.set("owner", Json::str(n.owner.clone()));
                        o.set(
                            "original",
                            n.original.clone().map(Json::Str).unwrap_or(Json::Null),
                        );
                        o.set(
                            "last_written",
                            n.last_written.clone().map(Json::Str).unwrap_or(Json::Null),
                        );
                        o.set("released", Json::Bool(n.released));
                        o.set("note", n.note.clone().map(Json::Str).unwrap_or(Json::Null));
                        o
                    })
                    .collect(),
            ),
        );
        j.set(
            "conflicts",
            Json::Arr(self.conflicts.iter().cloned().map(Json::Str).collect()),
        );
        j.set(
            "journal",
            Json::Arr(self.journal.iter().map(|e| e.to_json()).collect()),
        );
        j.set(
            "recovered",
            Json::Arr(self.recovered.iter().cloned().map(Json::Str).collect()),
        );
        j
    }
}

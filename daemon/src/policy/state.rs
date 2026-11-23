//! The state machine itself: dwell time, cooldown and a bounded transition log.
//!
//! Every transition carries the reason it happened. Without that, a policy engine is
//! impossible to debug on a phone you cannot attach a debugger to.

use crate::types::PolicyState;

#[derive(Debug, Clone)]
pub struct Transition {
    pub ts_ms: u64,
    pub from: PolicyState,
    pub to: PolicyState,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct StateMachine {
    current: PolicyState,
    since_ms: u64,
    last_change_ms: u64,
    cooldown_ms: u64,
    log: Vec<Transition>,
    boot_ms: u64,
}

const MAX_LOG: usize = 256;

impl StateMachine {
    pub fn new(boot_ms: u64, cooldown_ms: u64) -> Self {
        Self {
            current: PolicyState::Boot,
            since_ms: boot_ms,
            last_change_ms: boot_ms,
            cooldown_ms,
            log: Vec::new(),
            boot_ms,
        }
    }

    pub fn current(&self) -> PolicyState {
        self.current
    }

    pub fn since_ms(&self) -> u64 {
        self.since_ms
    }

    pub fn boot_ms(&self) -> u64 {
        self.boot_ms
    }

    pub fn set_cooldown(&mut self, ms: u64) {
        self.cooldown_ms = ms;
    }

    pub fn dwell_ms(&self, now: u64) -> u64 {
        now.saturating_sub(self.since_ms)
    }

    /// Cooldown only gates *de-escalation*: leaving a protective state too early is
    /// what causes the 2 Hz flapping that makes tuning modules feel worse than stock.
    pub fn in_cooldown(&self, now: u64) -> bool {
        now.saturating_sub(self.last_change_ms) < self.cooldown_ms
    }

    pub fn transition(&mut self, to: PolicyState, now: u64, reason: impl Into<String>) -> bool {
        if to == self.current {
            return false;
        }
        let reason = reason.into();
        self.log.push(Transition {
            ts_ms: now,
            from: self.current,
            to,
            reason: reason.clone(),
        });
        if self.log.len() > MAX_LOG {
            self.log.remove(0);
        }
        self.current = to;
        self.since_ms = now;
        self.last_change_ms = now;
        true
    }

    pub fn log(&self) -> &[Transition] {
        &self.log
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_are_logged_and_deduped() {
        let mut sm = StateMachine::new(0, 5_000);
        assert_eq!(sm.current(), PolicyState::Boot);
        assert!(sm.transition(PolicyState::Balanced, 1_000, "boot grace over"));
        assert!(!sm.transition(PolicyState::Balanced, 2_000, "again"));
        assert_eq!(sm.log().len(), 1);
        assert_eq!(sm.dwell_ms(4_000), 3_000);
    }

    #[test]
    fn cooldown_blocks_rapid_changes() {
        let mut sm = StateMachine::new(0, 5_000);
        sm.transition(PolicyState::Foreground, 1_000, "app");
        assert!(sm.in_cooldown(3_000));
        assert!(!sm.in_cooldown(7_000));
    }
}

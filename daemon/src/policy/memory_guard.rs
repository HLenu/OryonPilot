//! Memory guard: turns raw counters into a single "is the device struggling?" verdict.
//!
//! Swap usage alone is never a problem. A phone with 2 GiB in zRAM and PSI `full` at
//! zero is healthy; a phone with 300 MiB swapped and PSI `full` climbing is thrashing.
//! The distinction is made here so the policy engine never has to guess.

use crate::config::MemoryPolicy;
use crate::types::{MemSnapshot, PsiSnapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryClass {
    /// No reclaim pressure worth acting on.
    Normal,
    /// Reclaim is happening but the device still makes forward progress.
    Reclaiming,
    /// Sustained paging in + full stalls: the classic thrash signature.
    Thrashing,
}

impl MemoryClass {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryClass::Normal => "normal",
            MemoryClass::Reclaiming => "reclaiming",
            MemoryClass::Thrashing => "thrashing",
        }
    }
}

pub struct MemoryGuard {
    policy: MemoryPolicy,
    class: MemoryClass,
    pressured: bool,
}

impl MemoryGuard {
    pub fn new(policy: MemoryPolicy) -> Self {
        Self {
            policy,
            class: MemoryClass::Normal,
            pressured: false,
        }
    }

    pub fn class(&self) -> MemoryClass {
        self.class
    }

    pub fn is_pressured(&self) -> bool {
        self.pressured
    }

    pub fn update(&mut self, mem: &MemSnapshot, psi: &PsiSnapshot) -> MemoryClass {
        let p = &self.policy;
        let avail_pct = mem.available_pct();

        let enter = psi.memory.some10 > p.psi_some_enter
            || psi.memory.full10 > p.psi_full_enter
            || (mem.meminfo_ok && avail_pct < p.avail_pct_enter)
            || mem.kswapd_cpu_pct > p.kswapd_pct_enter;

        let exit = psi.memory.some10 < p.psi_some_exit
            && psi.memory.full10 < p.psi_full_exit
            && (!mem.meminfo_ok || avail_pct > p.avail_pct_exit)
            && mem.kswapd_cpu_pct < p.kswapd_pct_exit;

        if enter {
            self.pressured = true;
        } else if self.pressured && exit {
            self.pressured = false;
        }

        let thrashing = mem.swap_in_rate_kb_s > p.swap_in_rate_enter
            && psi.memory.full10 > 1.0
            && mem.pswpin > 0.0;
        self.class = if thrashing {
            MemoryClass::Thrashing
        } else if self.pressured || mem.kswapd_cpu_pct > p.kswapd_pct_exit {
            MemoryClass::Reclaiming
        } else {
            MemoryClass::Normal
        };
        self.class
    }

    pub fn describe(&self, mem: &MemSnapshot, psi: &PsiSnapshot) -> String {
        format!(
            "class={} some10={:.2} full10={:.2} avail={:.1}% kswapd={:.1}% swap_in={:.0}KiB/s",
            self.class.as_str(),
            psi.memory.some10,
            psi.memory.full10,
            mem.available_pct(),
            mem.kswapd_cpu_pct,
            mem.swap_in_rate_kb_s
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::types::{PsiRes, ZramDisk};

    fn mem(avail_pct: f64, kswapd: f64, swap_in: f64) -> MemSnapshot {
        MemSnapshot {
            total_kb: 11_534_336.0,
            available_kb: 11_534_336.0 * avail_pct / 100.0,
            kswapd_cpu_pct: kswapd,
            swap_in_rate_kb_s: swap_in,
            pswpin: 100.0,
            meminfo_ok: true,
            zram: vec![ZramDisk {
                name: "zram0".into(),
                orig_kb: 1024.0,
                compr_kb: 256.0,
                mem_used_kb: 300.0,
                limit_kb: 0.0,
                ratio: 4.0,
            }],
            ..Default::default()
        }
    }

    fn psi(some10: f64, full10: f64) -> PsiSnapshot {
        PsiSnapshot {
            memory: PsiRes {
                available: true,
                some10,
                full10,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn healthy_device_with_swap_is_not_pressured() {
        let mut g = MemoryGuard::new(Config::default().policy.memory);
        // 60% of zRAM used, no stalls at all.
        assert_eq!(
            g.update(&mem(35.0, 0.5, 0.0), &psi(0.4, 0.0)),
            MemoryClass::Normal
        );
        assert!(!g.is_pressured());
    }

    #[test]
    fn high_psi_enters_pressure() {
        let mut g = MemoryGuard::new(Config::default().policy.memory);
        assert_eq!(
            g.update(&mem(30.0, 2.0, 0.0), &psi(18.0, 1.0)),
            MemoryClass::Reclaiming
        );
        assert!(g.is_pressured());
    }

    #[test]
    fn hysteresis_keeps_pressure_until_all_signals_drop() {
        let mut g = MemoryGuard::new(Config::default().policy.memory);
        g.update(&mem(6.0, 30.0, 0.0), &psi(20.0, 5.0));
        assert!(g.is_pressured());
        // PSI dropped but available memory is still below the exit threshold.
        g.update(&mem(9.0, 2.0, 0.0), &psi(1.0, 0.1));
        assert!(g.is_pressured());
        // Everything below exit thresholds now.
        g.update(&mem(20.0, 2.0, 0.0), &psi(1.0, 0.1));
        assert!(!g.is_pressured());
    }

    #[test]
    fn thrashing_needs_swap_in_plus_stalls() {
        let mut g = MemoryGuard::new(Config::default().policy.memory);
        assert_eq!(
            g.update(&mem(5.0, 40.0, 20_000.0), &psi(30.0, 8.0)),
            MemoryClass::Thrashing
        );
        let mut g2 = MemoryGuard::new(Config::default().policy.memory);
        // Paging out (cache eviction) without paging in is not thrashing.
        assert_ne!(
            g2.update(&mem(5.0, 40.0, 0.0), &psi(30.0, 8.0)),
            MemoryClass::Thrashing
        );
    }

    #[test]
    fn missing_meminfo_does_not_crash() {
        let mut g = MemoryGuard::new(Config::default().policy.memory);
        let m = MemSnapshot::default();
        let p = PsiSnapshot::default();
        assert_eq!(g.update(&m, &p), MemoryClass::Normal);
    }
}

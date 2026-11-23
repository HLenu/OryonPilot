//! Battery / charger state from `/sys/class/power_supply`.
//!
//! Battery temperature is taken from here rather than from a thermal zone when
//! available: the power-supply driver reports the pack sensor directly.

use crate::sys::SysRoot;
use crate::types::BatterySnapshot;

const PS: &str = "/sys/class/power_supply";

pub fn collect(root: &SysRoot) -> BatterySnapshot {
    let mut snap = BatterySnapshot::default();
    let entries = root.list(PS);
    let mut battery_dir: Option<String> = None;
    let mut usb_online = false;
    let mut ac_online = false;

    for name in &entries {
        let base = format!("{PS}/{name}");
        let kind = root
            .read_trim(&format!("{base}/type"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        match kind.as_str() {
            "battery" if battery_dir.is_none() => battery_dir = Some(base.clone()),
            "usb" | "usb_pd" | "usbpd" => {
                usb_online |= root.read_i64(&format!("{base}/online")).unwrap_or(0) == 1;
            }
            "mains" => {
                ac_online |= root.read_i64(&format!("{base}/online")).unwrap_or(0) == 1;
            }
            _ => {}
        }
    }

    let Some(base) = battery_dir else {
        return snap;
    };
    snap.present = true;
    snap.capacity_pct = root
        .read_f64(&format!("{base}/capacity"))
        .filter(|v| (0.0..=100.0).contains(v));
    snap.temp_c = root
        .read_f64(&format!("{base}/temp"))
        // Android power_supply reports tenths of a degree Celsius (352 == 35.2 C).
        .map(|t| t / 10.0)
        .filter(|t| (-40.0..=125.0).contains(t));
    snap.status = root
        .read_trim(&format!("{base}/status"))
        .unwrap_or_default();
    // current_now is microamps. Sign conventions depend on the battery driver.
    snap.current_ma = root
        .read_f64(&format!("{base}/current_now"))
        .map(|v| v / 1000.0);
    snap.voltage_mv = root
        .read_f64(&format!("{base}/voltage_now"))
        .map(|v| v / 1000.0)
        .filter(|v| *v > 0.0);
    snap.charging = usb_online
        || ac_online
        || snap.status.eq_ignore_ascii_case("charging")
        || snap.status.eq_ignore_ascii_case("full");
    snap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_battery_and_charger() {
        let dir = std::env::temp_dir().join("neurosched-batt-fixture");
        let _ = std::fs::remove_dir_all(&dir);
        let b = dir.join("sys/class/power_supply/battery");
        let u = dir.join("sys/class/power_supply/usb");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::create_dir_all(&u).unwrap();
        std::fs::write(b.join("type"), "Battery").unwrap();
        std::fs::write(b.join("capacity"), "73").unwrap();
        std::fs::write(b.join("temp"), "352").unwrap();
        std::fs::write(b.join("status"), "Charging").unwrap();
        std::fs::write(b.join("current_now"), "1500000").unwrap();
        std::fs::write(b.join("voltage_now"), "4200000").unwrap();
        std::fs::write(u.join("type"), "USB").unwrap();
        std::fs::write(u.join("online"), "1").unwrap();

        let s = collect(&SysRoot::at(&dir));
        assert!(s.present);
        assert_eq!(s.capacity_pct, Some(73.0));
        assert_eq!(s.temp_c, Some(35.2));
        assert!(s.charging);
        assert_eq!(s.current_ma, Some(1500.0));
    }

    #[test]
    fn absent_battery_is_not_fatal() {
        let s = collect(&SysRoot::at(
            std::env::temp_dir().join("neurosched-batt-none"),
        ));
        assert!(!s.present);
        assert!(s.capacity_pct.is_none());
    }

    #[test]
    fn discharging_and_not_charging_are_not_charging() {
        let dir = std::env::temp_dir().join("neurosched-batt-status");
        let b = dir.join("sys/class/power_supply/battery");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(b.join("type"), "Battery").unwrap();
        for status in ["Discharging", "Not charging", "Unknown"] {
            std::fs::write(b.join("status"), status).unwrap();
            assert!(!collect(&SysRoot::at(&dir)).charging, "{status}");
        }
        for status in ["Charging", "Full"] {
            std::fs::write(b.join("status"), status).unwrap();
            assert!(collect(&SysRoot::at(&dir)).charging, "{status}");
        }
    }

    #[test]
    fn invalid_battery_values_are_unknown() {
        let dir = std::env::temp_dir().join("neurosched-batt-invalid");
        let b = dir.join("sys/class/power_supply/battery");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(b.join("type"), "Battery").unwrap();
        let root = SysRoot::at(&dir);
        for capacity in ["NaN", "inf", "-1", "101"] {
            std::fs::write(b.join("capacity"), capacity).unwrap();
            assert!(collect(&root).capacity_pct.is_none());
        }
        std::fs::write(b.join("current_now"), "NaN").unwrap();
        std::fs::write(b.join("voltage_now"), "-1").unwrap();
        assert!(collect(&root).current_ma.is_none());
        assert!(collect(&root).voltage_mv.is_none());
    }
}

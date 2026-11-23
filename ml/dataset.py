#!/usr/bin/env python3
"""Unified time-ordered dataset & zero-leakage feature pipeline for OryonPilot ML studies."""

from __future__ import annotations

import json
import math
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import pandas as pd

ROOT = Path(__file__).resolve().parent
DATA_DIR = ROOT / "data"

LAUNCHERS = {"-", "com.miui.home", "com.android.launcher3"}
SYSTEM_APPS = {"com.android.settings", "com.android.systemui"}

ACTION_NAMES = [
    "DEEP_SAVE",     # 0: uclamp 0 / 512
    "EFFICIENCY",    # 1: uclamp 0 / 768
    "RESPONSIVE",    # 2: uclamp 128 / 1024
    "BOOST",         # 3: uclamp 320 / 1024
    "THERMAL_CAP",   # 4: uclamp 0 / 640
]
ACTION_UCLAMP = {
    0: (0, 512),
    1: (0, 768),
    2: (128, 1024),
    3: (320, 1024),
    4: (0, 640),
}

SEQ_LEN = 15
TOTAL_RAM_MB = 15113.4

# Per-step features for sequential models (TCN / GRU / Transformer)
SEQ_FEATURES = [
    "cpu_pct",
    "prime_mhz_norm",
    "mem_avail_pct",
    "swap_used_gb",
    "zram_ratio",
    "psi_some10",
    "psi_full10",
    "kswapd_pct",
    "cpu_temp_c",
    "batt_temp_c",
    "temp_diff_cpu_batt",
    "batt_pct_norm",
    "is_charging",
    "is_launcher",
    "hour_sin",
    "hour_cos",
    "d_cpu_1m",
    "d_temp_1m",
    "d_mem_1m",
    "app_cpu_mean",
    "app_cpu_p90",
    "app_temp_mean",
]


@dataclass
class DatasetBundle:
    df_valid: pd.DataFrame
    X_tab: np.ndarray
    tab_feature_names: list[str]
    X_seq: np.ndarray
    seq_feature_names: list[str]
    y_cpu_next: np.ndarray
    y_burst: np.ndarray
    y_t1: np.ndarray
    y_t3: np.ndarray
    y_overheat_3m: np.ndarray
    y_action: np.ndarray
    y_uclamp_min: np.ndarray
    y_uclamp_max: np.ndarray
    train_idx: np.ndarray
    val_idx: np.ndarray
    test_idx: np.ndarray
    app_profiles: dict[str, dict]
    anon_map: dict[str, str]
    # Transition subset (Task 4): indices within df_valid where next foreground app != current non-launcher app
    trans_train_idx: np.ndarray
    trans_val_idx: np.ndarray
    trans_test_idx: np.ndarray
    y_next_tier: np.ndarray
    y_next_app_cls: np.ndarray
    top_apps: list[str]


def load_raw_df() -> pd.DataFrame:
    df = pd.read_csv(DATA_DIR / "history.csv")
    df = df[df["ts_iso"] != "ts_iso"].copy()
    df["ts"] = pd.to_datetime(df["ts_iso"], utc=True, errors="coerce")
    df = df.dropna(subset=["ts"]).sort_values("ts").reset_index(drop=True)
    num_cols = [
        "cpu_pct",
        "prime_mhz",
        "mem_avail_mb",
        "swap_used_mb",
        "zram_ratio",
        "psi_some10",
        "psi_full10",
        "kswapd_pct",
        "cpu_temp_c",
        "batt_temp_c",
        "batt_pct",
    ]
    for c in num_cols:
        df[c] = pd.to_numeric(df[c], errors="coerce")
    df = df.dropna(subset=["cpu_pct", "prime_mhz", "cpu_temp_c", "mem_avail_mb"]).reset_index(drop=True)
    df["foreground"] = df["foreground"].fillna("-").astype(str)
    df["dt_sec"] = df["ts"].diff().dt.total_seconds().fillna(60.0)
    df["contiguous"] = df["dt_sec"] <= 180.0
    df["seg_id"] = (~df["contiguous"]).cumsum()

    df["ts_local"] = df["ts"] + pd.Timedelta(hours=8)
    hour_f = df["ts_local"].dt.hour + df["ts_local"].dt.minute / 60.0
    df["hour_sin"] = np.sin(2.0 * np.pi * hour_f / 24.0)
    df["hour_cos"] = np.cos(2.0 * np.pi * hour_f / 24.0)
    dow_f = df["ts_local"].dt.dayofweek
    df["dow_sin"] = np.sin(2.0 * np.pi * dow_f / 7.0)
    df["dow_cos"] = np.cos(2.0 * np.pi * dow_f / 7.0)

    df["prime_mhz_norm"] = df["prime_mhz"] / 1000.0
    df["mem_avail_pct"] = (df["mem_avail_mb"] / TOTAL_RAM_MB * 100.0).clip(0.0, 100.0)
    df["swap_used_gb"] = df["swap_used_mb"] / 1024.0
    df["temp_diff_cpu_batt"] = df["cpu_temp_c"] - df["batt_temp_c"]
    df["batt_pct_norm"] = df["batt_pct"] / 100.0
    df["is_launcher"] = df["foreground"].isin(LAUNCHERS).astype(np.float32)

    # Charging estimation purely from past/current battery change (zero future leakage)
    batt_diff1 = df.groupby("seg_id")["batt_pct"].diff().fillna(0.0)
    batt_diff10 = df.groupby("seg_id")["batt_pct"].diff(10).fillna(0.0)
    df["is_charging"] = ((batt_diff1 > 0) | (batt_diff10 > 0)).astype(np.float32)

    # Past-only 1-min deltas within contiguous segment
    df["d_cpu_1m"] = df.groupby("seg_id")["cpu_pct"].diff().fillna(0.0)
    df["d_temp_1m"] = df.groupby("seg_id")["cpu_temp_c"].diff().fillna(0.0)
    df["d_mem_1m"] = df.groupby("seg_id")["mem_avail_mb"].diff().fillna(0.0)
    df["d_prime_1m"] = df.groupby("seg_id")["prime_mhz_norm"].diff().fillna(0.0)
    df["d_batt_temp_1m"] = df.groupby("seg_id")["batt_temp_c"].diff().fillna(0.0)
    return df


def build_dataset() -> DatasetBundle:
    df = load_raw_df()
    n = len(df)
    seg = df["seg_id"].values

    # Valid aligned sample positions: require t-(SEQ_LEN-1) .. t+3 in the same contiguous segment
    valid_mask = np.zeros(n, dtype=bool)
    for i in range(SEQ_LEN - 1, n - 3):
        if seg[i - (SEQ_LEN - 1)] == seg[i] and seg[i] == seg[i + 3]:
            valid_mask[i] = True

    raw_valid_positions = np.where(valid_mask)[0]
    nv = len(raw_valid_positions)
    n_train = int(nv * 0.70)
    n_val = int(nv * 0.15)

    train_end_raw_idx = int(raw_valid_positions[n_train - 1])

    # Anonymization map across dataset
    counts = df["foreground"].value_counts()
    anon_map: dict[str, str] = {}
    app_i = 1
    for pkg in counts.index:
        if pkg in LAUNCHERS or pkg in SYSTEM_APPS:
            anon_map[pkg] = pkg
        else:
            anon_map[pkg] = f"app_{app_i:03d}"
            app_i += 1

    # Compute app profiles strictly from TRAIN period (<= train_end_raw_idx) -> zero leakage!
    train_slice = df.iloc[: train_end_raw_idx + 1]
    global_cpu_mean = float(train_slice["cpu_pct"].mean())
    global_cpu_p90 = float(train_slice["cpu_pct"].quantile(0.90))
    global_temp_mean = float(train_slice["cpu_temp_c"].mean())

    app_profiles: dict[str, dict] = {}
    for pkg, grp in train_slice.groupby("foreground"):
        if len(grp) < 3:
            continue
        dt_mean = float(grp["d_temp_1m"].mean())
        c_mean = float(grp["cpu_pct"].mean())
        tier = 0 if c_mean < 8.0 else (1 if c_mean < 16.0 else 2)
        app_profiles[pkg] = {
            "anon": anon_map.get(pkg, pkg),
            "count": int(len(grp)),
            "cpu_mean": c_mean,
            "cpu_p90": float(grp["cpu_pct"].quantile(0.90)),
            "temp_mean": float(grp["cpu_temp_c"].mean()),
            "dtemp_mean": dt_mean,
            "tier": tier,
        }

    def prof_val(pkg: str, key: str, default: float) -> float:
        p = app_profiles.get(pkg)
        if p is None:
            return default
        return float(p[key])

    df["app_cpu_mean"] = df["foreground"].map(lambda p: prof_val(p, "cpu_mean", global_cpu_mean))
    df["app_cpu_p90"] = df["foreground"].map(lambda p: prof_val(p, "cpu_p90", global_cpu_p90))
    df["app_temp_mean"] = df["foreground"].map(lambda p: prof_val(p, "temp_mean", global_temp_mean))
    df["app_dtemp_mean"] = df["foreground"].map(lambda p: prof_val(p, "dtemp_mean", 0.0))
    df["app_freq_log"] = df["foreground"].map(
        lambda p: math.log1p(prof_val(p, "count", 0.0))
    )

    # Precompute past-only rolling stats within segments
    g = df.groupby("seg_id")
    for w in (3, 5, 10, 15):
        df[f"cpu_mean_{w}"] = g["cpu_pct"].transform(lambda s: s.rolling(w, min_periods=1).mean())
        df[f"cpu_max_{w}"] = g["cpu_pct"].transform(lambda s: s.rolling(w, min_periods=1).max())
        df[f"cpu_std_{w}"] = g["cpu_pct"].transform(lambda s: s.rolling(w, min_periods=1).std()).fillna(0.0)
        df[f"temp_mean_{w}"] = g["cpu_temp_c"].transform(lambda s: s.rolling(w, min_periods=1).mean())
        df[f"temp_max_{w}"] = g["cpu_temp_c"].transform(lambda s: s.rolling(w, min_periods=1).max())
        df[f"prime_mean_{w}"] = g["prime_mhz_norm"].transform(lambda s: s.rolling(w, min_periods=1).mean())
        df[f"psi_some_mean_{w}"] = g["psi_some10"].transform(lambda s: s.rolling(w, min_periods=1).mean())

    for lag in (1, 2, 3, 5):
        df[f"cpu_lag_{lag}"] = g["cpu_pct"].shift(lag).bfill()
        df[f"temp_lag_{lag}"] = g["cpu_temp_c"].shift(lag).bfill()
        df[f"prime_lag_{lag}"] = g["prime_mhz_norm"].shift(lag).bfill()

    df["temp_slope_3m"] = (df["cpu_temp_c"] - df["temp_lag_3"]) / 3.0
    df["cpu_slope_3m"] = (df["cpu_pct"] - df["cpu_lag_3"]) / 3.0
    df["fg_run_len"] = 1.0
    # Consecutive minutes on current foreground app
    fg_vals = df["foreground"].values
    run_lens = np.ones(n, dtype=np.float32)
    for i in range(1, n):
        if seg[i] == seg[i - 1] and fg_vals[i] == fg_vals[i - 1]:
            run_lens[i] = run_lens[i - 1] + 1.0
    df["fg_run_len"] = run_lens
    df["fg_changed"] = ((df["foreground"] != g["foreground"].shift(1).fillna(df["foreground"]))).astype(np.float32)

    tab_cols = [
        "cpu_pct",
        "prime_mhz_norm",
        "mem_avail_pct",
        "swap_used_gb",
        "zram_ratio",
        "psi_some10",
        "psi_full10",
        "kswapd_pct",
        "cpu_temp_c",
        "batt_temp_c",
        "temp_diff_cpu_batt",
        "batt_pct_norm",
        "is_charging",
        "is_launcher",
        "hour_sin",
        "hour_cos",
        "dow_sin",
        "dow_cos",
        "d_cpu_1m",
        "d_temp_1m",
        "d_mem_1m",
        "d_prime_1m",
        "d_batt_temp_1m",
        "temp_slope_3m",
        "cpu_slope_3m",
        "fg_run_len",
        "fg_changed",
        "app_cpu_mean",
        "app_cpu_p90",
        "app_temp_mean",
        "app_dtemp_mean",
        "app_freq_log",
    ]
    for w in (3, 5, 10, 15):
        tab_cols.extend([
            f"cpu_mean_{w}",
            f"cpu_max_{w}",
            f"cpu_std_{w}",
            f"temp_mean_{w}",
            f"temp_max_{w}",
            f"prime_mean_{w}",
            f"psi_some_mean_{w}",
        ])
    for lag in (1, 2, 3, 5):
        tab_cols.extend([f"cpu_lag_{lag}", f"temp_lag_{lag}", f"prime_lag_{lag}"])

    # Targets from t+1, t+2, t+3
    cpu_arr = df["cpu_pct"].values
    prime_arr = df["prime_mhz"].values
    temp_arr = df["cpu_temp_c"].values
    psi_full_arr = df["psi_full10"].values
    mem_pct_arr = df["mem_avail_pct"].values
    batt_arr = df["batt_pct"].values
    chg_arr = df["is_charging"].values
    launcher_arr = df["is_launcher"].values

    seq_mat = df[SEQ_FEATURES].values.astype(np.float32)
    tab_mat = df[tab_cols].values.astype(np.float32)

    X_tab_list = []
    X_seq_list = []
    y_cpu_next = np.zeros(nv, dtype=np.float32)
    y_burst = np.zeros(nv, dtype=np.int64)
    y_t1 = np.zeros(nv, dtype=np.float32)
    y_t3 = np.zeros(nv, dtype=np.float32)
    y_overheat_3m = np.zeros(nv, dtype=np.int64)
    y_action = np.zeros(nv, dtype=np.int64)
    y_uclamp_min = np.zeros(nv, dtype=np.float32)
    y_uclamp_max = np.zeros(nv, dtype=np.float32)

    for k, i in enumerate(raw_valid_positions):
        X_tab_list.append(tab_mat[i])
        X_seq_list.append(seq_mat[i - SEQ_LEN + 1 : i + 1])
        c1 = float(cpu_arr[i + 1])
        p1 = float(prime_arr[i + 1])
        t0 = float(temp_arr[i])
        t1 = float(temp_arr[i + 1])
        t2 = float(temp_arr[i + 2])
        t3 = float(temp_arr[i + 3])
        tmax3 = max(t1, t2, t3)
        tmax_all = max(t0, tmax3)

        y_cpu_next[k] = c1
        is_burst = int((c1 >= 25.0) or (p1 >= 2400.0))
        y_burst[k] = is_burst
        y_t1[k] = t1
        y_t3[k] = t3
        is_overheat = int(tmax3 >= 75.0)
        y_overheat_3m[k] = is_overheat

        # Physically grounded 5-class Oracle scheduling label
        mem_bad = (psi_full_arr[i] >= 3.0) or (psi_full_arr[i + 1] >= 3.0) or (mem_pct_arr[i] < 8.0)
        if is_overheat or (t0 >= 75.0 and t1 >= 70.0) or mem_bad:
            act = 4  # THERMAL_CAP
        elif (
            (chg_arr[i] < 0.5 and batt_arr[i] <= 15.0)
            or (
                launcher_arr[i] > 0.5
                and cpu_arr[i] < 4.5
                and c1 < 6.0
                and prime_arr[i] <= 1200.0
                and p1 <= 1400.0
            )
        ):
            act = 0  # DEEP_SAVE
        elif is_burst and tmax_all < 70.0:
            act = 3  # BOOST
        elif (c1 >= 10.0) or (launcher_arr[i] < 0.5 and cpu_arr[i] >= 5.0):
            act = 2  # RESPONSIVE
        else:
            act = 1  # EFFICIENCY

        y_action[k] = act
        umin, umax = ACTION_UCLAMP[act]
        y_uclamp_min[k] = float(umin)
        y_uclamp_max[k] = float(umax)

    X_tab = np.asarray(X_tab_list, dtype=np.float32)
    X_seq = np.asarray(X_seq_list, dtype=np.float32)
    df_valid = df.iloc[raw_valid_positions].copy().reset_index(drop=True)
    df_valid["anon_fg"] = df_valid["foreground"].map(lambda p: anon_map.get(p, p))
    df_valid["y_cpu_next"] = y_cpu_next
    df_valid["y_burst"] = y_burst
    df_valid["y_t1"] = y_t1
    df_valid["y_t3"] = y_t3
    df_valid["y_overheat_3m"] = y_overheat_3m
    df_valid["y_action"] = y_action

    train_idx = np.arange(0, n_train, dtype=np.int64)
    val_idx = np.arange(n_train, n_train + n_val, dtype=np.int64)
    test_idx = np.arange(n_train + n_val, nv, dtype=np.int64)

    # Task 4: De-launcherized next-app & load-tier transition dataset
    # Find next non-launcher foreground app within the same segment (up to 15 min lookahead)
    top_train_apps = (
        train_slice[~train_slice["foreground"].isin(LAUNCHERS)]["foreground"]
        .value_counts()
        .head(20)
        .index.tolist()
    )
    top_map = {pkg: idx for idx, pkg in enumerate(top_train_apps)}
    other_cls = len(top_train_apps)

    y_next_tier = np.full(nv, -1, dtype=np.int64)
    y_next_app_cls = np.full(nv, -1, dtype=np.int64)
    valid_fg = df_valid["foreground"].values
    valid_seg = df_valid["seg_id"].values
    for k in range(nv - 1):
        cur_p = valid_fg[k]
        if cur_p in LAUNCHERS:
            continue
        # Look ahead for first non-launcher app != cur_p in same segment
        for j in range(k + 1, min(nv, k + 15)):
            if valid_seg[j] != valid_seg[k]:
                break
            nxt = valid_fg[j]
            if nxt in LAUNCHERS or nxt == cur_p:
                continue
            tier = int(prof_val(nxt, "tier", 1))
            y_next_tier[k] = tier
            y_next_app_cls[k] = top_map.get(nxt, other_cls)
            break

    trans_train = np.array([i for i in train_idx if y_next_tier[i] >= 0], dtype=np.int64)
    trans_val = np.array([i for i in val_idx if y_next_tier[i] >= 0], dtype=np.int64)
    trans_test = np.array([i for i in test_idx if y_next_tier[i] >= 0], dtype=np.int64)

    return DatasetBundle(
        df_valid=df_valid,
        X_tab=X_tab,
        tab_feature_names=tab_cols,
        X_seq=X_seq,
        seq_feature_names=SEQ_FEATURES,
        y_cpu_next=y_cpu_next,
        y_burst=y_burst,
        y_t1=y_t1,
        y_t3=y_t3,
        y_overheat_3m=y_overheat_3m,
        y_action=y_action,
        y_uclamp_min=y_uclamp_min,
        y_uclamp_max=y_uclamp_max,
        train_idx=train_idx,
        val_idx=val_idx,
        test_idx=test_idx,
        app_profiles=app_profiles,
        anon_map=anon_map,
        trans_train_idx=trans_train,
        trans_val_idx=trans_val,
        trans_test_idx=trans_test,
        y_next_tier=y_next_tier,
        y_next_app_cls=y_next_app_cls,
        top_apps=top_train_apps,
    )


if __name__ == "__main__":
    ds = build_dataset()
    print(
        f"Dataset built: total={len(ds.df_valid)} "
        f"(train={len(ds.train_idx)}, val={len(ds.val_idx)}, test={len(ds.test_idx)})"
    )
    print(f"X_tab={ds.X_tab.shape}, X_seq={ds.X_seq.shape}")
    print(
        f"Burst rate: train={ds.y_burst[ds.train_idx].mean():.3f}, "
        f"val={ds.y_burst[ds.val_idx].mean():.3f}, test={ds.y_burst[ds.test_idx].mean():.3f}"
    )
    print(
        f"Overheat-3m rate: train={ds.y_overheat_3m[ds.train_idx].mean():.3f}, "
        f"val={ds.y_overheat_3m[ds.val_idx].mean():.3f}, test={ds.y_overheat_3m[ds.test_idx].mean():.3f}"
    )
    dist = {ACTION_NAMES[i]: int((ds.y_action == i).sum()) for i in range(len(ACTION_NAMES))}
    print(f"Action dist: {dist}")
    print(
        f"Transitions (Task 4): train={len(ds.trans_train_idx)}, "
        f"val={len(ds.trans_val_idx)}, test={len(ds.trans_test_idx)}"
    )

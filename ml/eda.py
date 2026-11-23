#!/usr/bin/env python3
"""Deep Exploratory Data Analysis (EDA) on 17-day Xiaomi 15 (SM8750) telemetry."""

from __future__ import annotations

import json
import math
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np
import pandas as pd
from sklearn.cluster import KMeans
from sklearn.linear_model import Ridge
from sklearn.metrics import r2_score, silhouette_score
from sklearn.preprocessing import StandardScaler

ROOT = Path(__file__).resolve().parent
DATA_DIR = ROOT / "data"
RES_DIR = ROOT / "results"
RES_DIR.mkdir(parents=True, exist_ok=True)

SYSTEM_KEEP = {
    "-",
    "com.miui.home",
    "com.android.settings",
    "com.android.systemui",
    "com.android.launcher3",
}


def build_anonymizer(packages: pd.Series | list[str]) -> dict[str, str]:
    counts: dict[str, int] = {}
    for p in packages:
        if p and p not in SYSTEM_KEEP:
            counts[p] = counts.get(p, 0) + 1
    ranked = sorted(counts.items(), key=lambda x: (-x[1], x[0]))
    mapping = {pkg: f"app_{idx:03d}" for idx, (pkg, _) in enumerate(ranked, 1)}
    for s in SYSTEM_KEEP:
        mapping[s] = s
    return mapping


def load_history() -> pd.DataFrame:
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
    # Local time CST (+8)
    df["ts_local"] = df["ts"] + pd.Timedelta(hours=8)
    df["hour"] = df["ts_local"].dt.hour
    df["dow"] = df["ts_local"].dt.dayofweek
    # Estimate charging vs discharging from battery percentage trend over 10-min rolling window
    df["dbatt"] = df["batt_pct"].diff()
    # Continuous segments (gap <= 180s)
    df["contiguous"] = df["dt_sec"] <= 180.0
    df["seg_id"] = (~df["contiguous"]).cumsum()
    # Charging inference: positive battery rise over local 15-min window or battery temp bump with rise
    df["batt_gain_10m"] = (
        df.groupby("seg_id")["batt_pct"].diff(10).fillna(0.0)
    )
    df["is_charging_est"] = (df["batt_gain_10m"] > 0) | (
        df.groupby("seg_id")["batt_pct"].diff().fillna(0.0) > 0
    )
    return df


def quantiles(s: pd.Series) -> dict[str, float]:
    qs = [0.0, 0.05, 0.25, 0.5, 0.75, 0.9, 0.95, 0.97, 0.99, 1.0]
    vals = s.quantile(qs)
    keys = ["min", "p05", "p25", "p50", "p75", "p90", "p95", "p97", "p99", "max"]
    return {k: round(float(vals.iloc[i]), 3) for i, k in enumerate(keys)} | {
        "mean": round(float(s.mean()), 3),
        "std": round(float(s.std()), 3),
    }


def analyze_transitions(model_data: dict, anon: dict[str, str]) -> dict:
    markov = model_data.get("markov", {}).get("table", {})
    raw_total = 0
    raw_top1_correct = 0
    raw_cond_entropy = 0.0
    for src, dsts in markov.items():
        row_sum = sum(dsts.values())
        if row_sum == 0:
            continue
        raw_total += row_sum
        raw_top1_correct += max(dsts.values())
        h = -sum((c / row_sum) * math.log2(c / row_sum) for c in dsts.values() if c > 0)
        raw_cond_entropy += row_sum * h
    raw_cond_entropy /= max(raw_total, 1)

    launchers = {"com.miui.home", "com.android.launcher3"}
    no_home_total = 0
    no_home_top1 = 0
    no_home_h = 0.0
    for src, dsts in markov.items():
        if src in launchers:
            continue
        filtered = {k: v for k, v in dsts.items() if k not in launchers}
        row_sum = sum(filtered.values())
        if row_sum == 0:
            continue
        no_home_total += row_sum
        no_home_top1 += max(filtered.values())
        h = -sum((c / row_sum) * math.log2(c / row_sum) for c in filtered.values() if c > 0)
        no_home_h += row_sum * h
    no_home_h /= max(no_home_total, 1)

    launches = model_data.get("launches", {})
    total_l = sum(launches.values())
    non_home_l = {k: v for k, v in launches.items() if k not in launchers}
    non_home_total = sum(non_home_l.values())
    top_non_home = sorted(non_home_l.items(), key=lambda x: (-x[1], x[0]))[:15]

    return {
        "events_total": int(model_data.get("events", 0)),
        "unique_packages": len(launches),
        "home_share_of_launches": round(launches.get("com.miui.home", 0) / max(total_l, 1), 4),
        "raw_markov_transitions": raw_total,
        "raw_markov_oracle_top1_upper_bound": round(raw_top1_correct / max(raw_total, 1), 4),
        "raw_markov_cond_entropy_bits": round(raw_cond_entropy, 4),
        "no_home_markov_transitions": no_home_total,
        "no_home_oracle_top1_upper_bound": round(no_home_top1 / max(no_home_total, 1), 4),
        "no_home_cond_entropy_bits": round(no_home_h, 4),
        "top15_non_home_share": round(sum(v for _, v in top_non_home) / max(non_home_total, 1), 4),
        "top15_non_home_anon": [
            {"app": anon.get(k, "app_other"), "launches": int(v), "share": round(v / max(non_home_total, 1), 4)}
            for k, v in top_non_home
        ],
    }


def analyze_thermodynamics(df: pd.DataFrame) -> dict:
    work = df.copy()
    work["next_temp"] = work.groupby("seg_id")["cpu_temp_c"].shift(-1)
    work["next3_max_temp"] = (
        work.groupby("seg_id")["cpu_temp_c"]
        .transform(lambda s: s.shift(-1).rolling(3, min_periods=1).max().shift(-2))
    )
    work["d_temp_1m"] = work["next_temp"] - work["cpu_temp_c"]
    work["temp_minus_batt"] = work["cpu_temp_c"] - work["batt_temp_c"]
    work["prev_temp"] = work.groupby("seg_id")["cpu_temp_c"].shift(1)
    work["d_temp_prev"] = work["cpu_temp_c"] - work["prev_temp"]
    sub = work.dropna(subset=["d_temp_1m", "d_temp_prev"]).copy()

    feats = [
        "cpu_temp_c",
        "temp_minus_batt",
        "cpu_pct",
        "prime_mhz",
        "d_temp_prev",
        "psi_some10",
    ]
    X = sub[feats].values
    y = sub["d_temp_1m"].values
    reg = Ridge(alpha=1.0).fit(X, y)
    pred = reg.predict(X)
    r2_dt = r2_score(y, pred)
    r2_t1 = r2_score(sub["next_temp"].values, sub["cpu_temp_c"].values + pred)
    mae_t1 = float(np.mean(np.abs(sub["next_temp"].values - (sub["cpu_temp_c"].values + pred))))
    persist_mae_t1 = float(np.mean(np.abs(sub["next_temp"].values - sub["cpu_temp_c"].values)))

    # Precursors 1-3 min before reaching >= 75C from < 75C
    onsets = work[(work["cpu_temp_c"] < 75.0) & (work["next_temp"] >= 75.0)]
    normal = work[(work["cpu_temp_c"] < 75.0) & (work["next_temp"] < 75.0)]

    return {
        "delta_temp_ridge_r2": round(float(r2_dt), 4),
        "next_temp_ridge_r2": round(float(r2_t1), 4),
        "next_temp_ridge_mae_c": round(mae_t1, 3),
        "next_temp_persistence_mae_c": round(persist_mae_t1, 3),
        "ridge_coeffs": {f: round(float(c), 6) for f, c in zip(feats, reg.coef_)},
        "ridge_intercept": round(float(reg.intercept_), 4),
        "heat_onset_75c_count": int(len(onsets)),
        "heat_onset_precursor_means": {
            "cpu_pct": round(float(onsets["cpu_pct"].mean()), 2) if len(onsets) else 0.0,
            "prime_mhz": round(float(onsets["prime_mhz"].mean()), 1) if len(onsets) else 0.0,
            "cpu_temp_c": round(float(onsets["cpu_temp_c"].mean()), 2) if len(onsets) else 0.0,
            "d_temp_prev": round(float(onsets["d_temp_prev"].mean()), 2) if len(onsets) else 0.0,
        },
        "non_onset_baseline_means": {
            "cpu_pct": round(float(normal["cpu_pct"].mean()), 2),
            "prime_mhz": round(float(normal["prime_mhz"].mean()), 1),
            "cpu_temp_c": round(float(normal["cpu_temp_c"].mean()), 2),
            "d_temp_prev": round(float(normal["d_temp_prev"].mean()), 2),
        },
    }


def analyze_waste_and_regimes(df: pd.DataFrame) -> dict:
    # Wasted prime frequency: cpu_pct < 12% yet prime_mhz >= 2000 MHz
    low_load = df["cpu_pct"] < 12.0
    high_prime = df["prime_mhz"] >= 2000.0
    waste = df[low_load & high_prime]
    very_low_waste = df[(df["cpu_pct"] < 5.0) & (df["prime_mhz"] >= 1800.0)]

    # Estimated discharge drain rate (% per hour) in non-charging segments >= 15 min
    disch = df[(~df["is_charging_est"]) & (df["dt_sec"] <= 90.0)].copy()
    disch["drain_pct_per_hr"] = (-disch["batt_pct"].diff().clip(upper=0)) * (3600.0 / disch["dt_sec"].clip(lower=1.0))

    # Unsupervised K-Means on standardized telemetry
    clust_cols = [
        "cpu_pct",
        "prime_mhz",
        "mem_avail_mb",
        "psi_some10",
        "cpu_temp_c",
        "batt_temp_c",
        "kswapd_pct",
    ]
    scaler = StandardScaler()
    Xs = scaler.fit_transform(df[clust_cols].values)
    # Sample for silhouette speed
    rng = np.random.default_rng(42)
    idx = rng.choice(len(Xs), size=min(4000, len(Xs)), replace=False)
    kmeans_results = {}
    best_labels = None
    best_k = 5
    for k in [4, 5, 6]:
        km = KMeans(n_clusters=k, n_init=10, random_state=42)
        labels = km.fit_predict(Xs)
        sil = float(silhouette_score(Xs[idx], labels[idx]))
        centers = scaler.inverse_transform(km.cluster_centers_)
        clusters_info = []
        for cid in range(k):
            mask = labels == cid
            cdict = {col: round(float(centers[cid, j]), 2) for j, col in enumerate(clust_cols)}
            cdict["share"] = round(float(mask.mean()), 4)
            cdict["count"] = int(mask.sum())
            clusters_info.append(cdict)
        clusters_info.sort(key=lambda d: d["cpu_pct"])
        kmeans_results[f"k={k}"] = {"silhouette": round(sil, 4), "clusters": clusters_info}
        if k == 5:
            best_labels = labels

    return {
        "low_cpu_lt12_share": round(float(low_load.mean()), 4),
        "high_prime_ge2000_share": round(float(high_prime.mean()), 4),
        "waste_low_cpu_high_prime_share": round(float(len(waste) / len(df)), 4),
        "waste_low_cpu_high_prime_count": int(len(waste)),
        "waste_very_low_lt5_prime_ge1800_count": int(len(very_low_waste)),
        "mean_discharge_pct_per_hr_overall": round(float(disch["drain_pct_per_hr"].mean()), 2),
        "mean_discharge_pct_per_hr_when_high_prime": round(
            float(disch[disch["prime_mhz"] >= 2000.0]["drain_pct_per_hr"].mean()), 2
        ),
        "mean_discharge_pct_per_hr_when_low_prime": round(
            float(disch[disch["prime_mhz"] < 1500.0]["drain_pct_per_hr"].mean()), 2
        ),
        "kmeans_unsupervised": kmeans_results,
    }


def make_plots(df: pd.DataFrame, summary: dict) -> None:
    plt.style.use("dark_background")

    # Plot 1: Diurnal profile (24h CST)
    fig, ax1 = plt.subplots(figsize=(10, 4.5), dpi=130)
    hourly = df.groupby("hour").agg(
        cpu=("cpu_pct", "mean"),
        temp=("cpu_temp_c", "mean"),
        prime=("prime_mhz", "mean"),
        n=("cpu_pct", "count"),
    )
    ax1.plot(hourly.index, hourly["cpu"], marker="o", color="#4cc2ff", label="CPU % (mean)")
    ax1.set_xlabel("Hour of Day (CST)")
    ax1.set_ylabel("CPU %", color="#4cc2ff")
    ax1.set_xticks(range(0, 24, 2))
    ax2 = ax1.twinx()
    ax2.plot(hourly.index, hourly["temp"], marker="s", color="#ff6b6b", label="CPU Temp C (mean)")
    ax2.set_ylabel("CPU Temp (C)", color="#ff6b6b")
    plt.title("Xiaomi 15 (17-Day Real Telemetry) - Diurnal CPU Load & Thermal Profile")
    fig.tight_layout()
    fig.savefig(RES_DIR / "eda_diurnal_temp.png")
    plt.close(fig)

    # Plot 2: CPU% vs Prime MHz & CPU Temp scatter/hexbin
    fig, axes = plt.subplots(1, 2, figsize=(11, 4.5), dpi=130)
    axes[0].hexbin(df["cpu_pct"], df["prime_mhz"], gridsize=35, mincnt=1, cmap="viridis")
    axes[0].axvline(12.0, color="#ff6b6b", linestyle="--", linewidth=1)
    axes[0].axhline(2000.0, color="#ff6b6b", linestyle="--", linewidth=1)
    axes[0].set_xlabel("CPU Total %")
    axes[0].set_ylabel("Prime Cluster MHz")
    axes[0].set_title("CPU Load vs Prime Freq (Wasted Boost Zone Top-Left)")

    axes[1].hexbin(df["cpu_pct"], df["cpu_temp_c"], gridsize=35, mincnt=1, cmap="magma")
    axes[1].axhline(75.0, color="#ffc857", linestyle="--", linewidth=1, label="75C Moderate")
    axes[1].axhline(83.0, color="#ff6b6b", linestyle="--", linewidth=1, label="83C Severe")
    axes[1].set_xlabel("CPU Total %")
    axes[1].set_ylabel("CPU Die Temp (C)")
    axes[1].set_title("CPU Load vs Die Temperature")
    axes[1].legend(loc="lower right", fontsize=8)
    fig.tight_layout()
    fig.savefig(RES_DIR / "eda_thermal_coupling.png")
    plt.close(fig)


def main() -> None:
    df = load_history()
    with open(DATA_DIR / "model.json", encoding="utf-8") as f:
        model_data = json.load(f)

    anon = build_anonymizer(
        list(df["foreground"].unique()) + list(model_data.get("launches", {}).keys())
    )
    df["fg_anon"] = df["foreground"].map(lambda p: anon.get(p, "app_other"))

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
    corr = df[num_cols].corr().round(3).to_dict()

    # Top 20 apps in history.csv anonymized profiling
    app_stats = (
        df.groupby("fg_anon")
        .agg(
            minutes=("cpu_pct", "count"),
            cpu_mean=("cpu_pct", "mean"),
            cpu_p90=("cpu_pct", lambda s: s.quantile(0.9)),
            prime_mean=("prime_mhz", "mean"),
            temp_mean=("cpu_temp_c", "mean"),
            temp_p95=("cpu_temp_c", lambda s: s.quantile(0.95)),
            psi_full_mean=("psi_full10", "mean"),
            mem_avail_mean=("mem_avail_mb", "mean"),
        )
        .sort_values("minutes", ascending=False)
        .head(20)
        .round(2)
        .to_dict(orient="index")
    )

    # Burst persistence / autocorrelation
    burst = (df["cpu_pct"] >= 25.0) | (df["prime_mhz"] >= 2400.0)
    cont_df = df.copy()
    cont_df["burst"] = burst.astype(int)
    cont_df["next_burst"] = cont_df.groupby("seg_id")["burst"].shift(-1)
    valid_b = cont_df.dropna(subset=["next_burst"])
    p_burst_given_burst = float(valid_b[valid_b["burst"] == 1]["next_burst"].mean())
    p_burst_given_calm = float(valid_b[valid_b["burst"] == 0]["next_burst"].mean())

    summary = {
        "rows_valid": int(len(df)),
        "time_start_utc": df["ts"].min().isoformat(),
        "time_end_utc": df["ts"].max().isoformat(),
        "span_days": round((df["ts"].max() - df["ts"].min()).total_seconds() / 86400.0, 2),
        "contiguous_segments": int(df["seg_id"].nunique()),
        "dt_sec_quantiles": quantiles(df["dt_sec"]),
        "distributions": {c: quantiles(df[c]) for c in num_cols},
        "state_counts_recorded": df["state"].value_counts().to_dict(),
        "burst_rate": round(float(burst.mean()), 4),
        "p_burst_next_given_burst": round(p_burst_given_burst, 4),
        "p_burst_next_given_calm": round(p_burst_given_calm, 4),
        "correlations": corr,
        "thermodynamics": analyze_thermodynamics(df),
        "waste_and_regimes": analyze_waste_and_regimes(df),
        "app_transitions": analyze_transitions(model_data, anon),
        "top20_fg_profiles_anon": app_stats,
    }

    with open(RES_DIR / "eda_summary.json", "w", encoding="utf-8") as f:
        json.dump(summary, f, indent=2, ensure_ascii=False)
    make_plots(df, summary)
    print(f"EDA complete: {summary['rows_valid']} rows over {summary['span_days']} days.")
    print(f"Burst rate: {summary['burst_rate']:.1%}, P(burst|burst)={summary['p_burst_next_given_burst']:.3f}, P(burst|calm)={summary['p_burst_next_given_calm']:.3f}")
    print(f"Wasted low-CPU(<12%)+high-prime(>=2GHz) share: {summary['waste_and_regimes']['waste_low_cpu_high_prime_share']:.1%}")
    print(f"Thermodynamics next-min T Ridge MAE: {summary['thermodynamics']['next_temp_ridge_mae_c']}C vs Persistence {summary['thermodynamics']['next_temp_persistence_mae_c']}C")
    print(f"Transitions no-home Oracle Top-1 upper bound: {summary['app_transitions']['no_home_oracle_top1_upper_bound']:.1%}, entropy={summary['app_transitions']['no_home_cond_entropy_bits']} bits")


if __name__ == "__main__":
    main()

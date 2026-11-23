#!/usr/bin/env python3
"""Aggregate all Stage 1-2 EDA, Tabular, Neural, Qwen-0.5B LoRA, and RL simulation findings."""

from __future__ import annotations

import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent
RES_DIR = ROOT / "results"


def load(name: str) -> dict:
    p = RES_DIR / name
    if not p.exists():
        return {}
    with open(p, encoding="utf-8") as f:
        return json.load(f)


def main() -> None:
    eda = load("eda_summary.json")
    tab = load("tabular_results.json")
    neu = load("neural_results.json")
    cgbdt = load("compact_gbdt_results.json")
    qwen = load("qwen_lora_results.json")
    rl = load("rl_sim_results.json")

    print("=== 1. EDA HIGHLIGHTS ===")
    if eda:
        print(f"Rows: {eda['rows_valid']}, Span days: {eda['span_days']}, Segments: {eda['contiguous_segments']}")
        print(f"Burst rate: {eda['burst_rate']:.1%}, P(burst|burst)={eda['p_burst_next_given_burst']:.3f}, P(burst|calm)={eda['p_burst_next_given_calm']:.3f}")
        w = eda["waste_and_regimes"]
        print(f"Waste (<12% CPU & >=2GHz prime): {w['waste_low_cpu_high_prime_share']:.1%} ({w['waste_low_cpu_high_prime_count']} mins)")
        t = eda["thermodynamics"]
        print(f"Thermo 1m Ridge MAE: {t['next_temp_ridge_mae_c']}C vs Persist {t['next_temp_persistence_mae_c']}C | 75C onsets: {t['heat_onset_75c_count']}")
        tr = eda["app_transitions"]
        print(f"Transitions: home_share={tr['home_share_of_launches']:.1%}, no_home_oracle_top1={tr['no_home_oracle_top1_upper_bound']:.1%}, H={tr['no_home_cond_entropy_bits']}b")

    print("\n=== 2. BLIND TEST MODEL BENCHMARK ===")
    rows = []
    if tab:
        b = tab["baselines"]
        rows.append(("Persistence", b["Persistence"], "-"))
        rows.append(("Linear(Ridge/LR)", b["Linear_Ridge_LogReg"], f"{b['Linear_Ridge_LogReg'].get('cpu_latency_ms')}ms"))
        rows.append(("LightGBM", tab["gbdt"]["LightGBM"], f"{tab['gbdt']['LightGBM'].get('cpu_latency_ms')}ms/{tab['gbdt']['LightGBM'].get('model_bundle_kb')}KB"))
        rows.append(("HistGB", tab["gbdt"]["HistGradientBoosting"], "-"))
    if neu:
        for name in ["TCN", "GRU", "MiniTransformer", "TinyMLP", "TinyMLP_Distilled"]:
            if name in neu:
                m = neu[name]
                rows.append((name, m, f"{m['cpu_latency_ms']}ms/{m['weights_fp32_kb']}KB"))
    if cgbdt:
        rows.append(("CompactGBDT_FQI", cgbdt, f"<0.05ms/{cgbdt.get('json_kb')}KiB"))
    if qwen:
        if "zero_shot_250" in qwen:
            z = qwen["zero_shot_250"]
            rows.append(("Qwen2.5-0.5B-ZS(250)", z, f"json={z['json_valid_rate']:.0%} {z['ms_per_sample_gpu_batch']}ms/gpu"))
        if "lora_finetuned_full_test" in qwen:
            q = qwen["lora_finetuned_full_test"]
            rows.append(("Qwen2.5-0.5B-LoRA(full)", q, f"json={q['json_valid_rate']:.0%} {q['ms_per_sample_gpu_batch']}ms/gpu"))

    header = f"{'Model':24s} | {'CPU_MAE':>7s} | {'Burst_F1':>8s} | {'T+1_MAE':>7s} | {'T+3_MAE':>7s} | {'OH_F1/AUC':>11s} | {'Act_Acc':>7s} | {'Act_F1':>7s} | Footnote"
    print(header)
    print("-" * len(header))
    for name, m, note in rows:
        cpu_m = m.get("cpu_next", {}).get("mae", float("nan"))
        b_f1 = m.get("burst", {}).get("f1", float("nan"))
        t1_m = m.get("t1", {}).get("mae", float("nan"))
        t3_m = m.get("t3", {}).get("mae", float("nan"))
        oh_f1 = m.get("overheat_3m", {}).get("f1", float("nan"))
        oh_auc = m.get("overheat_3m", {}).get("roc_auc", None)
        oh_str = f"{oh_f1:.3f}/{oh_auc:.3f}" if oh_auc is not None else f"{oh_f1:.3f}/  -  "
        act_acc = m.get("action", {}).get("acc", float("nan"))
        act_f1 = m.get("action", {}).get("macro_f1", float("nan"))
        print(f"{name:24s} | {cpu_m:7.3f} | {b_f1:8.3f} | {t1_m:7.3f} | {t3_m:7.3f} | {oh_str:>11s} | {act_acc:7.3f} | {act_f1:7.3f} | {note}")

    if rl:
        print("\n=== 3. COUNTERFACTUAL & CONTINUOUS RL SIMULATION ===")
        for sec in ["blind_test_simulation", "full_17d_continuous_learning"]:
            print(f"[{sec}]")
            for k, v in rl.get(sec, {}).items():
                print(f"  {k:24s}: rew={v['mean_reward']:+.4f} perf={v['mean_perf_score']:+.4f} therm={v['mean_thermal_pen']:.4f} waste={v['mean_waste_pen']:.4f} OH%={v['overheat_ge75_pct']:.1f}% Q={v['quartile_mean_rewards']}")


if __name__ == "__main__":
    main()

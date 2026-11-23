#!/usr/bin/env python3
"""Family 1 (Statistical/Linear/Static Rules) & Family 2 (GBDT/LightGBM) evaluation."""

from __future__ import annotations

import json
import pickle
import time
from collections import Counter, defaultdict
from pathlib import Path

import lightgbm as lgb
import numpy as np
from sklearn.ensemble import HistGradientBoostingClassifier, HistGradientBoostingRegressor
from sklearn.linear_model import LogisticRegression, Ridge
from sklearn.metrics import (
    accuracy_score,
    average_precision_score,
    f1_score,
    mean_absolute_error,
    mean_squared_error,
    roc_auc_score,
)
from sklearn.preprocessing import StandardScaler

from dataset import ACTION_NAMES, LAUNCHERS, build_dataset

ROOT = Path(__file__).resolve().parent
RES_DIR = ROOT / "results"
ART_DIR = ROOT / "artifacts"
RES_DIR.mkdir(parents=True, exist_ok=True)
ART_DIR.mkdir(parents=True, exist_ok=True)


def reg_metrics(y_true: np.ndarray, y_pred: np.ndarray) -> dict[str, float]:
    mae = float(mean_absolute_error(y_true, y_pred))
    rmse = float(np.sqrt(mean_squared_error(y_true, y_pred)))
    return {"mae": round(mae, 4), "rmse": round(rmse, 4)}


def bin_metrics(y_true: np.ndarray, y_prob: np.ndarray, thresh: float = 0.5) -> dict[str, float]:
    y_prob = np.clip(y_prob, 1e-6, 1.0 - 1e-6)
    pred = (y_prob >= thresh).astype(int)
    auc = float(roc_auc_score(y_true, y_prob)) if len(np.unique(y_true)) > 1 else 0.0
    ap = float(average_precision_score(y_true, y_prob)) if len(np.unique(y_true)) > 1 else 0.0
    f1 = float(f1_score(y_true, pred, zero_division=0))
    acc = float(accuracy_score(y_true, pred))
    return {
        "roc_auc": round(auc, 4),
        "pr_auc": round(ap, 4),
        "f1": round(f1, 4),
        "acc": round(acc, 4),
    }


def cls_metrics(y_true: np.ndarray, y_pred: np.ndarray) -> dict[str, float]:
    return {
        "acc": round(float(accuracy_score(y_true, y_pred)), 4),
        "macro_f1": round(float(f1_score(y_true, y_pred, average="macro", zero_division=0)), 4),
        "weighted_f1": round(float(f1_score(y_true, y_pred, average="weighted", zero_division=0)), 4),
    }


def tune_binary_threshold(y_val: np.ndarray, p_val: np.ndarray) -> float:
    best_t, best_f1 = 0.5, -1.0
    for t in np.linspace(0.03, 0.75, 73):
        f1 = f1_score(y_val, (p_val >= t).astype(int), zero_division=0)
        if f1 > best_f1:
            best_f1 = f1
            best_t = float(t)
    return best_t


def eval_static_baselines(ds) -> dict:
    te = ds.test_idx
    df_te = ds.df_valid.iloc[te]

    cpu_cur = df_te["cpu_pct"].values
    t_cur = df_te["cpu_temp_c"].values
    prime_cur = df_te["prime_mhz"].values
    psi_full_cur = df_te["psi_full10"].values
    mem_pct_cur = df_te["mem_avail_pct"].values
    batt_cur = df_te["batt_pct"].values
    chg_cur = df_te["is_charging"].values
    launch_cur = df_te["is_launcher"].values

    # Persistence
    persist_cpu = reg_metrics(ds.y_cpu_next[te], cpu_cur)
    persist_t1 = reg_metrics(ds.y_t1[te], t_cur)
    persist_t3 = reg_metrics(ds.y_t3[te], t_cur)
    persist_burst_p = ((cpu_cur >= 25.0) | (prime_cur >= 2400.0)).astype(float)
    persist_oh_p = (t_cur >= 75.0).astype(float)

    # EWMA (mean of last 3 mins)
    ewma_cpu = reg_metrics(ds.y_cpu_next[te], df_te["cpu_mean_3"].values)
    ewma_t1 = reg_metrics(ds.y_t1[te], df_te["temp_mean_3"].values)
    ewma_t3 = reg_metrics(ds.y_t3[te], df_te["temp_mean_3"].values)

    # Static v0.1.2 rule proxy (mapped into 5 actions)
    rule_act = np.ones(len(te), dtype=np.int64)
    for k in range(len(te)):
        if t_cur[k] >= 75.0 or psi_full_cur[k] >= 3.0 or mem_pct_cur[k] < 8.0:
            rule_act[k] = 4
        elif (chg_cur[k] < 0.5 and batt_cur[k] <= 15.0) or (
            launch_cur[k] > 0.5 and cpu_cur[k] < 4.5 and prime_cur[k] <= 1200.0
        ):
            rule_act[k] = 0
        elif cpu_cur[k] >= 25.0 or prime_cur[k] >= 2400.0:
            rule_act[k] = 3
        elif launch_cur[k] < 0.5 and cpu_cur[k] >= 5.0:
            rule_act[k] = 2
        else:
            rule_act[k] = 1

    # Linear / Ridge / Logistic baseline (fit on train only)
    tr, va = ds.train_idx, ds.val_idx
    scaler = StandardScaler().fit(ds.X_tab[tr])
    Xtr = scaler.transform(ds.X_tab[tr])
    Xva = scaler.transform(ds.X_tab[va])
    Xte = scaler.transform(ds.X_tab[te])

    r_cpu = Ridge(alpha=10.0).fit(Xtr, ds.y_cpu_next[tr])
    r_t1 = Ridge(alpha=10.0).fit(Xtr, ds.y_t1[tr])
    r_t3 = Ridge(alpha=10.0).fit(Xtr, ds.y_t3[tr])

    lr_burst = LogisticRegression(max_iter=500, class_weight="balanced").fit(Xtr, ds.y_burst[tr])
    th_b = tune_binary_threshold(ds.y_burst[va], lr_burst.predict_proba(Xva)[:, 1])

    lr_oh = LogisticRegression(max_iter=500, class_weight="balanced").fit(Xtr, ds.y_overheat_3m[tr])
    th_o = tune_binary_threshold(ds.y_overheat_3m[va], lr_oh.predict_proba(Xva)[:, 1])

    lr_act = LogisticRegression(max_iter=600, class_weight="balanced").fit(Xtr, ds.y_action[tr])

    t0 = time.perf_counter()
    _ = lr_act.predict(Xte[:200])
    lin_ms = (time.perf_counter() - t0) * 1000.0 / 200.0

    return {
        "Persistence": {
            "cpu_next": persist_cpu,
            "burst": bin_metrics(ds.y_burst[te], persist_burst_p),
            "t1": persist_t1,
            "t3": persist_t3,
            "overheat_3m": bin_metrics(ds.y_overheat_3m[te], persist_oh_p),
            "action": cls_metrics(ds.y_action[te], rule_act),
        },
        "EWMA_3m": {
            "cpu_next": ewma_cpu,
            "t1": ewma_t1,
            "t3": ewma_t3,
        },
        "Linear_Ridge_LogReg": {
            "cpu_next": reg_metrics(ds.y_cpu_next[te], np.clip(r_cpu.predict(Xte), 0, 100)),
            "burst": bin_metrics(ds.y_burst[te], lr_burst.predict_proba(Xte)[:, 1], th_b),
            "t1": reg_metrics(ds.y_t1[te], r_t1.predict(Xte)),
            "t3": reg_metrics(ds.y_t3[te], r_t3.predict(Xte)),
            "overheat_3m": bin_metrics(ds.y_overheat_3m[te], lr_oh.predict_proba(Xte)[:, 1], th_o),
            "action": cls_metrics(ds.y_action[te], lr_act.predict(Xte)),
            "cpu_latency_ms": round(lin_ms, 4),
        },
    }


def eval_markov_task4(ds) -> dict:
    # Build 1st-order Markov over contiguous non-launcher app sequences in TRAIN ONLY
    train_fg = ds.df_valid.iloc[ds.train_idx]["foreground"].values
    train_seg = ds.df_valid.iloc[ds.train_idx]["seg_id"].values
    trans_counts: dict[str, Counter] = defaultdict(Counter)
    prev_app = None
    prev_seg = None
    for fg, sg in zip(train_fg, train_seg):
        if fg in LAUNCHERS:
            continue
        if prev_seg == sg and prev_app is not None and prev_app != fg:
            trans_counts[prev_app][fg] += 1
        prev_app = fg
        prev_seg = sg

    top_map = {pkg: idx for idx, pkg in enumerate(ds.top_apps)}
    other_cls = len(ds.top_apps)
    global_tier_mode = int(Counter(ds.y_next_tier[ds.trans_train_idx]).most_common(1)[0][0])

    top1_app_hits = 0
    top3_app_hits = 0
    tier_preds = []
    tier_trues = []
    test_fgs = ds.df_valid["foreground"].values
    for idx in ds.trans_test_idx:
        cur = test_fgs[idx]
        true_cls = int(ds.y_next_app_cls[idx])
        true_tier = int(ds.y_next_tier[idx])
        tier_trues.append(true_tier)

        row = trans_counts.get(cur)
        if not row:
            tier_preds.append(global_tier_mode)
            continue
        ranked = [pkg for pkg, _ in row.most_common(3)]
        cls_ranked = [top_map.get(pkg, other_cls) for pkg in ranked]
        if cls_ranked[0] == true_cls:
            top1_app_hits += 1
        if true_cls in cls_ranked:
            top3_app_hits += 1
        best_pkg = ranked[0]
        pred_tier = int(ds.app_profiles.get(best_pkg, {}).get("tier", global_tier_mode))
        tier_preds.append(pred_tier)

    n = max(len(ds.trans_test_idx), 1)
    return {
        "next_tier": cls_metrics(np.array(tier_trues), np.array(tier_preds)),
        "next_app_top1_acc": round(top1_app_hits / n, 4),
        "next_app_top3_acc": round(top3_app_hits / n, 4),
        "n_test": int(len(ds.trans_test_idx)),
    }


def eval_gbdt(ds) -> dict:
    tr, va, te = ds.train_idx, ds.val_idx, ds.test_idx
    Xtr, Xva, Xte = ds.X_tab[tr], ds.X_tab[va], ds.X_tab[te]

    def fit_lgb_reg(y):
        m = lgb.LGBMRegressor(
            n_estimators=800,
            learning_rate=0.03,
            num_leaves=31,
            subsample_for_bin=50000,
            colsample_bytree=0.85,
            subsample=0.85,
            min_child_samples=25,
            random_state=42,
            verbose=-1,
        )
        m.fit(
            Xtr,
            y[tr],
            eval_set=[(Xva, y[va])],
            callbacks=[lgb.early_stopping(40, verbose=False)],
        )
        return m

    def fit_lgb_bin(y):
        pos = max(int((y[tr] == 1).sum()), 1)
        neg = max(int((y[tr] == 0).sum()), 1)
        m = lgb.LGBMClassifier(
            n_estimators=800,
            learning_rate=0.03,
            num_leaves=31,
            scale_pos_weight=min(neg / pos, 2.0),
            colsample_bytree=0.85,
            subsample=0.85,
            min_child_samples=25,
            random_state=42,
            verbose=-1,
        )
        m.fit(
            Xtr,
            y[tr],
            eval_set=[(Xva, y[va])],
            eval_metric="auc",
            callbacks=[lgb.early_stopping(60, verbose=False)],
        )
        return m

    m_cpu = fit_lgb_reg(ds.y_cpu_next)
    m_t1 = fit_lgb_reg(ds.y_t1)
    m_t3 = fit_lgb_reg(ds.y_t3)
    m_burst = fit_lgb_bin(ds.y_burst)
    m_oh = fit_lgb_bin(ds.y_overheat_3m)

    m_act = lgb.LGBMClassifier(
        n_estimators=800,
        learning_rate=0.03,
        num_leaves=31,
        class_weight="balanced",
        colsample_bytree=0.85,
        subsample=0.85,
        min_child_samples=25,
        random_state=42,
        verbose=-1,
    )
    m_act.fit(
        Xtr,
        ds.y_action[tr],
        eval_set=[(Xva, ds.y_action[va])],
        callbacks=[lgb.early_stopping(40, verbose=False)],
    )

    th_b = tune_binary_threshold(ds.y_burst[va], m_burst.predict_proba(Xva)[:, 1])
    th_o = tune_binary_threshold(ds.y_overheat_3m[va], m_oh.predict_proba(Xva)[:, 1])

    # Task 4 GBDT on transition subset
    ttr, tva, tte = ds.trans_train_idx, ds.trans_val_idx, ds.trans_test_idx
    m_tier = lgb.LGBMClassifier(
        n_estimators=600,
        learning_rate=0.03,
        num_leaves=23,
        class_weight="balanced",
        min_child_samples=15,
        random_state=42,
        verbose=-1,
    )
    m_tier.fit(
        ds.X_tab[ttr],
        ds.y_next_tier[ttr],
        eval_set=[(ds.X_tab[tva], ds.y_next_tier[tva])],
        callbacks=[lgb.early_stopping(35, verbose=False)],
    )
    m_app = lgb.LGBMClassifier(
        n_estimators=500,
        learning_rate=0.03,
        num_leaves=23,
        min_child_samples=15,
        random_state=42,
        verbose=-1,
    )
    m_app.fit(
        ds.X_tab[ttr],
        ds.y_next_app_cls[ttr],
        eval_set=[(ds.X_tab[tva], ds.y_next_app_cls[tva])],
        callbacks=[lgb.early_stopping(35, verbose=False)],
    )
    app_probs = m_app.predict_proba(ds.X_tab[tte])
    top3_idx = np.argsort(-app_probs, axis=1)[:, :3]
    top1_hits = int((top3_idx[:, 0] == ds.y_next_app_cls[tte]).sum())
    top3_hits = int(sum(ds.y_next_app_cls[tte][i] in top3_idx[i] for i in range(len(tte))))

    # Measure inference latency (single sample CPU)
    t0 = time.perf_counter()
    for i in range(200):
        _ = m_act.predict(Xte[i : i + 1])
    lgb_ms = (time.perf_counter() - t0) * 1000.0 / 200.0

    # Save LightGBM models for sim_rl
    bundle_path = ART_DIR / "lgb_models.pkl"
    with open(bundle_path, "wb") as f:
        pickle.dump(
            {
                "cpu": m_cpu,
                "t1": m_t1,
                "t3": m_t3,
                "burst": m_burst,
                "overheat": m_oh,
                "action": m_act,
                "th_burst": th_b,
                "th_overheat": th_o,
            },
            f,
        )

    # Top-15 importances on action & overheat
    imp_act = sorted(
        zip(ds.tab_feature_names, m_act.feature_importances_),
        key=lambda x: -x[1],
    )[:15]
    imp_oh = sorted(
        zip(ds.tab_feature_names, m_oh.feature_importances_),
        key=lambda x: -x[1],
    )[:15]

    # HistGradientBoosting comparison
    hgb_cpu = HistGradientBoostingRegressor(max_iter=300, max_depth=6, random_state=42).fit(Xtr, ds.y_cpu_next[tr])
    hgb_act = HistGradientBoostingClassifier(max_iter=300, max_depth=6, class_weight="balanced", random_state=42).fit(Xtr, ds.y_action[tr])
    hgb_oh = HistGradientBoostingClassifier(max_iter=300, max_depth=6, class_weight="balanced", random_state=42).fit(Xtr, ds.y_overheat_3m[tr])
    th_hgb_o = tune_binary_threshold(ds.y_overheat_3m[va], hgb_oh.predict_proba(Xva)[:, 1])

    return {
        "LightGBM": {
            "cpu_next": reg_metrics(ds.y_cpu_next[te], np.clip(m_cpu.predict(Xte), 0, 100)),
            "burst": bin_metrics(ds.y_burst[te], m_burst.predict_proba(Xte)[:, 1], th_b),
            "t1": reg_metrics(ds.y_t1[te], m_t1.predict(Xte)),
            "t3": reg_metrics(ds.y_t3[te], m_t3.predict(Xte)),
            "overheat_3m": bin_metrics(ds.y_overheat_3m[te], m_oh.predict_proba(Xte)[:, 1], th_o),
            "action": cls_metrics(ds.y_action[te], m_act.predict(Xte)),
            "task4_next_tier": cls_metrics(ds.y_next_tier[tte], m_tier.predict(ds.X_tab[tte])),
            "task4_next_app_top1": round(top1_hits / max(len(tte), 1), 4),
            "task4_next_app_top3": round(top3_hits / max(len(tte), 1), 4),
            "cpu_latency_ms": round(lgb_ms, 4),
            "model_bundle_kb": round(bundle_path.stat().st_size / 1024.0, 1),
            "top_features_action": [{"feature": f, "importance": int(v)} for f, v in imp_act],
            "top_features_overheat": [{"feature": f, "importance": int(v)} for f, v in imp_oh],
        },
        "HistGradientBoosting": {
            "cpu_next": reg_metrics(ds.y_cpu_next[te], np.clip(hgb_cpu.predict(Xte), 0, 100)),
            "overheat_3m": bin_metrics(ds.y_overheat_3m[te], hgb_oh.predict_proba(Xte)[:, 1], th_hgb_o),
            "action": cls_metrics(ds.y_action[te], hgb_act.predict(Xte)),
        },
    }


def main() -> None:
    ds = build_dataset()
    results = {
        "baselines": eval_static_baselines(ds),
        "markov_task4": eval_markov_task4(ds),
        "gbdt": eval_gbdt(ds),
    }
    out_path = RES_DIR / "tabular_results.json"
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(results, f, indent=2, ensure_ascii=False)
    print("=== Tabular & Baselines Blind Test Summary ===")
    print("Persistence:", results["baselines"]["Persistence"])
    print("Linear:", results["baselines"]["Linear_Ridge_LogReg"])
    print("LightGBM:", {k: v for k, v in results["gbdt"]["LightGBM"].items() if not k.startswith("top_features")})
    print("Markov Task4:", results["markov_task4"])


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Train unified CompactGBDT + OfflineRL FQI model and export pure-JSON weights (<55 KiB) for Rust."""

from __future__ import annotations

import json
import math
from pathlib import Path

import lightgbm as lgb
import numpy as np
from sklearn.metrics import accuracy_score, f1_score

from dataset import (
    ACTION_NAMES,
    bin_metrics,
    build_dataset,
    cls_metrics,
    reg_metrics,
    tune_binary_threshold,
)
from sim_rl import build_compact_context, fit_fqi_policy, fit_thermal_env

ROOT = Path(__file__).resolve().parent
ART_DIR = ROOT / "artifacts"
RES_DIR = ROOT / "results"
ART_DIR.mkdir(parents=True, exist_ok=True)
RES_DIR.mkdir(parents=True, exist_ok=True)


def flatten_lgb_tree(node: dict, decimals: int = 5) -> dict:
    f_arr: list[int] = []
    t_arr: list[float | int] = []
    l_arr: list[int] = []
    r_arr: list[int] = []
    v_arr: list[float | int] = []

    def visit(n: dict) -> int:
        idx = len(f_arr)
        f_arr.append(-1)
        t_arr.append(0)
        l_arr.append(0)
        r_arr.append(0)
        v_arr.append(0)
        if "split_index" in n:
            assert n.get("decision_type", "<=") == "<=", f"Unsupported split type: {n.get('decision_type')}"
            f_arr[idx] = int(n["split_feature"])
            # Preserve exact IEEE-754 threshold float64 (LightGBM encodes strict < vs <= via ~1e-12 offsets)
            thr = float(n["threshold"])
            t_arr[idx] = int(thr) if thr.is_integer() else thr
            left_idx = visit(n["left_child"])
            right_idx = visit(n["right_child"])
            l_arr[idx] = left_idx
            r_arr[idx] = right_idx
        else:
            val = round(float(n["leaf_value"]), decimals)
            v_arr[idx] = 0 if val == 0.0 else val
        return idx

    visit(node)
    return {"f": f_arr, "t": t_arr, "l": l_arr, "r": r_arr, "v": v_arr}


def eval_tree(tree: dict, x: np.ndarray) -> float:
    f = tree["f"]
    t = tree["t"]
    l = tree["l"]
    r = tree["r"]
    v = tree["v"]
    idx = 0
    for _ in range(32):
        feat = f[idx]
        if feat < 0:
            return float(v[idx])
        idx = l[idx] if float(x[feat]) <= float(t[idx]) else r[idx]
    raise RuntimeError("Tree depth exceeded 32")


def eval_ensemble(trees: list[dict], X: np.ndarray) -> np.ndarray:
    out = np.zeros(len(X), dtype=np.float64)
    for row_i in range(len(X)):
        row = X[row_i]
        s = 0.0
        for tr in trees:
            s += eval_tree(tr, row)
        out[row_i] = s
    return out


def sigmoid(z: np.ndarray) -> np.ndarray:
    z_clip = np.clip(z, -40.0, 40.0)
    return 1.0 / (1.0 + np.exp(-z_clip))


def softmax(logits: np.ndarray) -> np.ndarray:
    m = logits - logits.max(axis=1, keepdims=True)
    e = np.exp(m)
    return e / e.sum(axis=1, keepdims=True)


def export_model_bundle(
    num_leaves: int,
    n_reg: int,
    n_act: int,
    use_mild_weights: bool,
    decimals: int,
    ds,
    fqi_weights: np.ndarray,
    ctx_all: np.ndarray,
) -> tuple[dict, dict, float]:
    tr, va, te = ds.train_idx, ds.val_idx, ds.test_idx
    Xtr, Xva, Xte = ds.X_tab[tr], ds.X_tab[va], ds.X_tab[te]

    def fit_reg(y):
        m = lgb.LGBMRegressor(
            n_estimators=n_reg,
            learning_rate=0.06,
            max_depth=4,
            num_leaves=num_leaves,
            min_child_samples=40,
            colsample_bytree=0.85,
            subsample=0.85,
            subsample_freq=1,
            random_state=42,
            verbose=-1,
        )
        m.fit(Xtr, y[tr])
        return m

    def fit_bin(y):
        pos = max(int((y[tr] == 1).sum()), 1)
        neg = max(int((y[tr] == 0).sum()), 1)
        m = lgb.LGBMClassifier(
            n_estimators=n_reg,
            learning_rate=0.06,
            max_depth=4,
            num_leaves=num_leaves,
            min_child_samples=40,
            scale_pos_weight=min(neg / pos, 3.0),
            colsample_bytree=0.85,
            subsample=0.85,
            subsample_freq=1,
            random_state=42,
            verbose=-1,
        )
        m.fit(Xtr, y[tr])
        return m

    m_cpu = fit_reg(ds.y_cpu_next)
    m_burst = fit_bin(ds.y_burst)
    m_t1 = fit_reg(ds.y_t1)
    m_t3 = fit_reg(ds.y_t3)
    m_oh = fit_bin(ds.y_overheat_3m)

    sample_weight = None
    if use_mild_weights:
        counts = np.bincount(ds.y_action[tr], minlength=len(ACTION_NAMES)).astype(np.float64)
        w = np.power(counts.sum() / (len(ACTION_NAMES) * np.maximum(counts, 1.0)), 0.35)
        w = np.clip(w, 0.75, 2.0)
        sample_weight = w[ds.y_action[tr]]

    m_act = lgb.LGBMClassifier(
        n_estimators=n_act,
        learning_rate=0.06,
        max_depth=4,
        num_leaves=num_leaves,
        min_child_samples=40,
        colsample_bytree=0.85,
        subsample=0.85,
        subsample_freq=1,
        random_state=42,
        verbose=-1,
    )
    m_act.fit(Xtr, ds.y_action[tr], sample_weight=sample_weight)

    th_b = tune_binary_threshold(ds.y_burst[va], m_burst.predict_proba(Xva)[:, 1])
    th_o = tune_binary_threshold(ds.y_overheat_3m[va], m_oh.predict_proba(Xva)[:, 1])

    # Dump and flatten trees
    def dump_single(model) -> list[dict]:
        info = model.booster_.dump_model()["tree_info"]
        return [flatten_lgb_tree(t["tree_structure"], decimals=decimals) for t in info]

    def dump_multiclass(model, n_cls: int = 5) -> list[list[dict]]:
        info = model.booster_.dump_model()["tree_info"]
        classes = [[] for _ in range(n_cls)]
        for i, t in enumerate(info):
            classes[i % n_cls].append(flatten_lgb_tree(t["tree_structure"], decimals=decimals))
        return classes

    heads = {
        "cpu": dump_single(m_cpu),
        "burst": dump_single(m_burst),
        "t1": dump_single(m_t1),
        "t3": dump_single(m_t3),
        "overheat": dump_single(m_oh),
        "action": dump_multiclass(m_act, len(ACTION_NAMES)),
    }

    # Strict Python parity check on FULL blind test set Xte
    raw_cpu = eval_ensemble(heads["cpu"], Xte)
    raw_b = eval_ensemble(heads["burst"], Xte)
    raw_t1 = eval_ensemble(heads["t1"], Xte)
    raw_t3 = eval_ensemble(heads["t3"], Xte)
    raw_oh = eval_ensemble(heads["overheat"], Xte)
    raw_act = np.column_stack([eval_ensemble(heads["action"][c], Xte) for c in range(len(ACTION_NAMES))])

    p_cpu = np.clip(raw_cpu, 0.0, 100.0)
    p_b = sigmoid(raw_b)
    p_t1 = raw_t1
    p_t3 = raw_t3
    p_oh = sigmoid(raw_oh)
    p_act_probs = softmax(raw_act)

    err_cpu = float(np.max(np.abs(p_cpu - np.clip(m_cpu.predict(Xte), 0.0, 100.0))))
    err_b = float(np.max(np.abs(p_b - m_burst.predict_proba(Xte)[:, 1])))
    err_t1 = float(np.max(np.abs(p_t1 - m_t1.predict(Xte))))
    err_t3 = float(np.max(np.abs(p_t3 - m_t3.predict(Xte))))
    err_oh = float(np.max(np.abs(p_oh - m_oh.predict_proba(Xte)[:, 1])))
    err_act = float(np.max(np.abs(p_act_probs - m_act.predict_proba(Xte))))
    max_err = max(err_cpu, err_b, err_t1, err_t3, err_oh, err_act)
    assert max_err < 1e-2, f"Exported tree parity error too high: {max_err}"

    df_tr = ds.df_valid.iloc[tr]
    global_priors = {
        "cpu_mean": round(float(df_tr["cpu_pct"].mean()), 4),
        "cpu_p90": round(float(df_tr["cpu_pct"].quantile(0.90)), 4),
        "temp_mean": round(float(df_tr["cpu_temp_c"].mean()), 4),
    }

    val_act_p = m_act.predict(Xva)
    val_score = float(
        f1_score(ds.y_action[va], val_act_p, average="macro", zero_division=0)
        + 0.30 * accuracy_score(ds.y_action[va], val_act_p)
    )

    test_metrics = {
        "cpu_next": reg_metrics(ds.y_cpu_next[te], p_cpu),
        "burst": bin_metrics(ds.y_burst[te], p_b, th_b),
        "t1": reg_metrics(ds.y_t1[te], p_t1),
        "t3": reg_metrics(ds.y_t3[te], p_t3),
        "overheat_3m": bin_metrics(ds.y_overheat_3m[te], p_oh, th_o),
        "action": cls_metrics(ds.y_action[te], np.argmax(p_act_probs, axis=1)),
        "max_tree_parity_err": round(max_err, 6),
        "val_score": round(val_score, 4),
        "num_leaves": num_leaves,
        "n_reg_trees": n_reg,
        "n_act_trees_per_class": n_act,
        "total_trees": sum(len(heads[k]) for k in ("cpu", "burst", "t1", "t3", "overheat"))
        + sum(len(c) for c in heads["action"]),
    }

    fqi_w_rounded = [[round(float(v), 5) for v in row] for row in fqi_weights]
    sample_local_positions = [0, len(te) // 2, len(te) - 1]
    parity_samples = []
    for lp in sample_local_positions:
        gi = int(te[lp])
        raw_list = [round(float(v), 7) for v in ds.X_tab[gi]]
        raw_arr = np.asarray([raw_list], dtype=np.float64)
        ctx = [round(float(v), 6) for v in ctx_all[gi]]
        fqi_q = [float(np.dot(fqi_w_rounded[a], ctx)) for a in range(len(ACTION_NAMES))]
        act_raw = np.array([[eval_ensemble(heads["action"][c], raw_arr)[0] for c in range(len(ACTION_NAMES))]])
        act_p = softmax(act_raw)[0]
        parity_samples.append({
            "raw_features": raw_list,
            "context_15": ctx,
            "expected_cpu": round(float(np.clip(eval_ensemble(heads["cpu"], raw_arr)[0], 0.0, 100.0)), 6),
            "expected_burst_prob": round(float(sigmoid(eval_ensemble(heads["burst"], raw_arr))[0]), 6),
            "expected_t1": round(float(eval_ensemble(heads["t1"], raw_arr)[0]), 6),
            "expected_t3": round(float(eval_ensemble(heads["t3"], raw_arr)[0]), 6),
            "expected_oh_prob": round(float(sigmoid(eval_ensemble(heads["overheat"], raw_arr))[0]), 6),
            "expected_action_probs": [round(float(v), 6) for v in act_p],
            "expected_fqi_q": [round(float(v), 6) for v in fqi_q],
        })

    payload = {
        "schema": 2,
        "architecture": "CompactGBDT_FQI",
        "feature_names": ds.tab_feature_names,
        "global_priors": global_priors,
        "action_names": ACTION_NAMES,
        "th_burst": round(th_b, 3),
        "th_overheat": round(th_o, 3),
        "heads": heads,
        "fqi": {"context_dim": int(fqi_weights.shape[1]), "weights": fqi_w_rounded},
        "test_metrics": test_metrics,
        "parity_samples": parity_samples,
    }
    return payload, test_metrics, val_score


def main() -> None:
    ds = build_dataset()
    env_reg, prime_coef, _ = fit_thermal_env(ds)
    ctx_all = build_compact_context(ds)
    fqi_models = fit_fqi_policy(ds, ctx_all, env_reg, prime_coef)
    fqi_weights = np.array([m.coef_ for m in fqi_models], dtype=np.float64)

    # Try weighted vs unweighted action head on Val, trimming tree budget if > 55 KiB
    best_payload = None
    best_val = -1e9
    for use_w in (False, True):
        for num_leaves, n_reg, n_act, dec in [
            (8, 18, 12, 4),
            (7, 18, 12, 4),
            (8, 16, 11, 4),
            (7, 16, 11, 4),
            (6, 15, 10, 4),
        ]:
            payload, metrics, val_score = export_model_bundle(
                num_leaves=num_leaves,
                n_reg=n_reg,
                n_act=n_act,
                use_mild_weights=use_w,
                decimals=dec,
                ds=ds,
                fqi_weights=fqi_weights,
                ctx_all=ctx_all,
            )
            raw = json.dumps(payload, separators=(",", ":")).encode("utf-8")
            kb = len(raw) / 1024.0
            if kb <= 54.0:
                payload["test_metrics"]["json_kb"] = round(kb, 2)
                payload["test_metrics"]["use_mild_weights"] = use_w
                if val_score > best_val:
                    best_val = val_score
                    best_payload = payload
                break

    assert best_payload is not None, "Could not fit CompactGBDT_FQI within 55 KiB budget"
    out_model = ART_DIR / "sched_model.json"
    out_model.write_text(json.dumps(best_payload, separators=(",", ":")), encoding="utf-8")
    out_res = RES_DIR / "compact_gbdt_results.json"
    out_res.write_text(json.dumps(best_payload["test_metrics"], indent=2), encoding="utf-8")

    tm = best_payload["test_metrics"]
    print(
        f"[CompactGBDT_FQI] size={tm['json_kb']} KiB | trees={tm['total_trees']} | "
        f"CPU_MAE={tm['cpu_next']['mae']} | T+1_MAE={tm['t1']['mae']} | T+3_MAE={tm['t3']['mae']} | "
        f"Burst_F1={tm['burst']['f1']} | OH_AUC={tm['overheat_3m']['roc_auc']} (F1={tm['overheat_3m']['f1']}) | "
        f"Act_Acc={tm['action']['acc']} Act_MacroF1={tm['action']['macro_f1']} | max_parity_err={tm['max_tree_parity_err']}"
    )


if __name__ == "__main__":
    main()

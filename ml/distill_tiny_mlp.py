#!/usr/bin/env python3
"""Knowledge Distillation from LightGBM Teacher -> Compact TinyMultiTaskMLP Student."""

from __future__ import annotations

import json
import pickle
import time
from pathlib import Path

import numpy as np
import torch
import torch.nn as nn
import torch.nn.functional as F
from sklearn.metrics import accuracy_score, f1_score, roc_auc_score
from torch.utils.data import DataLoader, TensorDataset

from dataset import ACTION_NAMES, build_dataset
from train_neural import (
    TinyMultiTaskMLP,
    bin_metrics,
    reg_metrics,
    set_seed,
    tune_thresh,
)

ROOT = Path(__file__).resolve().parent
RES_DIR = ROOT / "results"
ART_DIR = ROOT / "artifacts"


def round_list(arr: np.ndarray, decimals: int = 5) -> list:
    arr64 = np.round(np.asarray(arr, dtype=np.float64), decimals)
    if arr64.ndim == 1:
        return [round(float(x), decimals) for x in arr64]
    return [[round(float(v), decimals) for v in row] for row in arr64]


def export_compact_tiny_mlp(
    model: TinyMultiTaskMLP,
    mean: np.ndarray,
    std: np.ndarray,
    feat_names: list[str],
    metrics: dict,
    ds,
) -> Path:
    sd = model.state_dict()
    tr = ds.train_idx
    df_tr = ds.df_valid.iloc[tr]
    global_priors = {
        "cpu_mean": round(float(df_tr["cpu_pct"].mean()), 4),
        "cpu_p90": round(float(df_tr["cpu_pct"].quantile(0.90)), 4),
        "temp_mean": round(float(df_tr["cpu_temp_c"].mean()), 4),
    }

    te = ds.test_idx
    sample_positions = [int(te[0]), int(te[len(te) // 2]), int(te[-1])]
    parity_samples = []
    model.eval()
    denom = np.maximum(std, 1e-4)
    with torch.no_grad():
        for idx in sample_positions:
            raw_f = ds.X_tab[idx].astype(np.float64)
            raw_f_rounded = np.round(raw_f, 5)
            x_norm = ((raw_f_rounded - np.round(mean, 5)) / np.maximum(np.round(denom, 5), 1e-4)).astype(np.float32)
            out = model(torch.tensor(x_norm[None, :], dtype=torch.float32))
            probs = torch.softmax(out["act"][0], dim=-1).cpu().numpy()
            parity_samples.append({
                "raw_features": [round(float(v), 5) for v in raw_f_rounded],
                "expected_cpu": round(float(np.clip(out["cpu"][0].item(), 0.0, 100.0)), 6),
                "expected_burst_prob": round(float(torch.sigmoid(out["burst"][0]).item()), 6),
                "expected_t1": round(float(out["t1"][0].item()), 6),
                "expected_t3": round(float(out["t3"][0].item()), 6),
                "expected_oh_prob": round(float(torch.sigmoid(out["oh"][0]).item()), 6),
                "expected_action_probs": [round(float(v), 6) for v in probs],
            })

    payload = {
        "schema": 1,
        "architecture": "TinyMultiTaskMLP",
        "feature_names": feat_names,
        "scaler_mean": round_list(mean, 5),
        "scaler_std": round_list(std, 5),
        "global_priors": global_priors,
        "action_names": ACTION_NAMES,
        "th_burst": metrics["th_burst"],
        "th_overheat": metrics["th_overheat"],
        "layers": {
            "fc1_w": round_list(sd["fc1.weight"].detach().cpu().numpy(), 5),
            "fc1_b": round_list(sd["fc1.bias"].detach().cpu().numpy(), 5),
            "fc2_w": round_list(sd["fc2.weight"].detach().cpu().numpy(), 5),
            "fc2_b": round_list(sd["fc2.bias"].detach().cpu().numpy(), 5),
            "cpu_w": round_list(sd["head.cpu_head.weight"][0].detach().cpu().numpy(), 5),
            "cpu_b": round(float(sd["head.cpu_head.bias"][0].item()), 5),
            "burst_w": round_list(sd["head.burst_head.weight"][0].detach().cpu().numpy(), 5),
            "burst_b": round(float(sd["head.burst_head.bias"][0].item()), 5),
            "t1_w": round_list(sd["head.t1_head.weight"][0].detach().cpu().numpy(), 5),
            "t1_b": round(float(sd["head.t1_head.bias"][0].item()), 5),
            "t3_w": round_list(sd["head.t3_head.weight"][0].detach().cpu().numpy(), 5),
            "t3_b": round(float(sd["head.t3_head.bias"][0].item()), 5),
            "oh_w": round_list(sd["head.oh_head.weight"][0].detach().cpu().numpy(), 5),
            "oh_b": round(float(sd["head.oh_head.bias"][0].item()), 5),
            "act_w": round_list(sd["head.act_head.weight"].detach().cpu().numpy(), 5),
            "act_b": round_list(sd["head.act_head.bias"].detach().cpu().numpy(), 5),
        },
        "test_metrics": metrics,
        "parity_samples": parity_samples,
    }
    out = ART_DIR / "tiny_mlp_weights.json"
    with open(out, "w", encoding="utf-8") as f:
        json.dump(payload, f, separators=(",", ":"))
    return out


def main() -> None:
    set_seed(42)
    device = torch.device("cpu")
    ds = build_dataset()
    tr, va, te = ds.train_idx, ds.val_idx, ds.test_idx

    with open(ART_DIR / "lgb_models.pkl", "rb") as f:
        lgb_bundle = pickle.load(f)

    # Teacher soft predictions on TRAIN only (zero leakage into Val/Test)
    X_raw_tr = ds.X_tab[tr]
    t_cpu = np.clip(lgb_bundle["cpu"].predict(X_raw_tr), 0.0, 100.0).astype(np.float32)
    t_t1 = lgb_bundle["t1"].predict(X_raw_tr).astype(np.float32)
    t_t3 = lgb_bundle["t3"].predict(X_raw_tr).astype(np.float32)
    t_burst_p = np.clip(lgb_bundle["burst"].predict_proba(X_raw_tr)[:, 1], 1e-4, 1 - 1e-4).astype(np.float32)
    t_oh_p = np.clip(lgb_bundle["overheat"].predict_proba(X_raw_tr)[:, 1], 1e-4, 1 - 1e-4).astype(np.float32)
    t_act_p = np.clip(lgb_bundle["action"].predict_proba(X_raw_tr), 1e-5, 1.0).astype(np.float32)
    t_act_p /= t_act_p.sum(axis=1, keepdims=True)

    tab_mean = ds.X_tab[tr].mean(axis=0)
    tab_std = np.maximum(ds.X_tab[tr].std(axis=0), 1e-4)
    X_tr = ((ds.X_tab[tr] - tab_mean) / tab_std).astype(np.float32)
    X_va = ((ds.X_tab[va] - tab_mean) / tab_std).astype(np.float32)
    X_te = ((ds.X_tab[te] - tab_mean) / tab_std).astype(np.float32)

    # Mild balanced weights (power 0.35 so rare actions improve without hurting overall accuracy)
    act_counts = np.bincount(ds.y_action[tr], minlength=len(ACTION_NAMES)).astype(np.float32)
    raw_w = act_counts.sum() / (len(ACTION_NAMES) * np.maximum(act_counts, 1.0))
    act_w = np.clip(np.power(raw_w, 0.35), 0.7, 2.2)

    b_pos = max(float((ds.y_burst[tr] == 1).sum()), 1.0)
    b_neg = max(float((ds.y_burst[tr] == 0).sum()), 1.0)
    o_pos = max(float((ds.y_overheat_3m[tr] == 1).sum()), 1.0)
    o_neg = max(float((ds.y_overheat_3m[tr] == 0).sum()), 1.0)

    c_mean = float(ds.y_cpu_next[tr].mean())
    c_std = max(float(ds.y_cpu_next[tr].std()), 1.0)
    t_mean = float(ds.y_t1[tr].mean())
    t_std = max(float(ds.y_t1[tr].std()), 1.0)

    loader = DataLoader(
        TensorDataset(
            torch.tensor(X_tr),
            torch.tensor(ds.y_cpu_next[tr], dtype=torch.float32),
            torch.tensor(ds.y_burst[tr], dtype=torch.float32),
            torch.tensor(ds.y_t1[tr], dtype=torch.float32),
            torch.tensor(ds.y_t3[tr], dtype=torch.float32),
            torch.tensor(ds.y_overheat_3m[tr], dtype=torch.float32),
            torch.tensor(ds.y_action[tr], dtype=torch.long),
            torch.tensor(t_cpu),
            torch.tensor(t_burst_p),
            torch.tensor(t_t1),
            torch.tensor(t_t3),
            torch.tensor(t_oh_p),
            torch.tensor(t_act_p),
        ),
        batch_size=128,
        shuffle=True,
    )

    X_va_t = torch.tensor(X_va)
    X_te_t = torch.tensor(X_te)

    # Tune distillation weight alpha & temperature T strictly on VAL
    candidates = [
        (0.50, 2.0),
        (0.65, 2.0),
        (0.80, 1.5),
    ]
    best_overall = None

    for alpha_distill, temp in candidates:
        set_seed(42)
        net = TinyMultiTaskMLP(X_tr.shape[1], h1=48, h2=24)
        opt = torch.optim.AdamW(net.parameters(), lr=2.5e-3, weight_decay=3e-4)
        epochs = 75
        sched = torch.optim.lr_scheduler.CosineAnnealingLR(opt, T_max=epochs)
        bce_b = nn.BCEWithLogitsLoss(pos_weight=torch.tensor([b_neg / b_pos]))
        bce_o = nn.BCEWithLogitsLoss(pos_weight=torch.tensor([o_neg / o_pos]))
        ce_a = nn.CrossEntropyLoss(weight=torch.tensor(act_w))
        huber = nn.SmoothL1Loss(beta=1.5)

        best_val = -1e9
        best_sd = None
        for _ep in range(epochs):
            net.train()
            for xb, yc, yb, yt1, yt3, yo, ya, tc, tb, tt1, tt3, to, ta in loader:
                out = net(xb)
                loss_cpu = 0.5 * huber((out["cpu"] - c_mean) / c_std, (yc - c_mean) / c_std) + 0.5 * huber((out["cpu"] - c_mean) / c_std, (tc - c_mean) / c_std)
                loss_t1 = 0.45 * huber((out["t1"] - t_mean) / t_std, (yt1 - t_mean) / t_std) + 0.55 * huber((out["t1"] - t_mean) / t_std, (tt1 - t_mean) / t_std)
                loss_t3 = 0.45 * huber((out["t3"] - t_mean) / t_std, (yt3 - t_mean) / t_std) + 0.55 * huber((out["t3"] - t_mean) / t_std, (tt3 - t_mean) / t_std)
                loss_b = 0.65 * bce_b(out["burst"], yb) + 0.35 * F.binary_cross_entropy_with_logits(out["burst"], tb)
                loss_o = 0.70 * bce_o(out["oh"], yo) + 0.30 * F.binary_cross_entropy_with_logits(out["oh"], to)

                ta_soft = torch.pow(ta, 1.0 / temp)
                ta_soft = ta_soft / ta_soft.sum(dim=-1, keepdim=True)
                kl_act = F.kl_div(F.log_softmax(out["act"] / temp, dim=-1), ta_soft, reduction="batchmean") * (temp * temp)
                loss_a = (1.0 - alpha_distill) * ce_a(out["act"], ya) + alpha_distill * kl_act

                loss = loss_cpu + 0.85 * loss_b + 1.15 * loss_t1 + 1.15 * loss_t3 + 0.90 * loss_o + 1.45 * loss_a
                opt.zero_grad()
                loss.backward()
                torch.nn.utils.clip_grad_norm_(net.parameters(), 2.0)
                opt.step()
            sched.step()

            net.eval()
            with torch.no_grad():
                vo = net(X_va_t)
                va_act = vo["act"].argmax(dim=-1).numpy()
                va_f1 = f1_score(ds.y_action[va], va_act, average="macro", zero_division=0)
                va_acc = accuracy_score(ds.y_action[va], va_act)
                va_oh_auc = roc_auc_score(ds.y_overheat_3m[va], torch.sigmoid(vo["oh"]).numpy())
                va_t1_mae = float(np.abs(vo["t1"].numpy() - ds.y_t1[va]).mean())
                score = va_f1 + 0.25 * va_acc + 0.4 * va_oh_auc - 0.03 * va_t1_mae
            if score > best_val:
                best_val = score
                best_sd = {k: v.detach().cpu().clone() for k, v in net.state_dict().items()}

        if best_overall is None or best_val > best_overall[0]:
            best_overall = (best_val, alpha_distill, temp, best_sd)

    _, best_alpha, best_temp, best_sd = best_overall
    net = TinyMultiTaskMLP(X_tr.shape[1], h1=48, h2=24)
    net.load_state_dict(best_sd)
    net.eval()

    with torch.no_grad():
        vo = net(X_va_t)
        th_b = tune_thresh(ds.y_burst[va], torch.sigmoid(vo["burst"]).numpy())
        th_o = tune_thresh(ds.y_overheat_3m[va], torch.sigmoid(vo["oh"]).numpy())

        to = net(X_te_t)
        p_cpu = np.clip(to["cpu"].numpy(), 0.0, 100.0)
        p_burst = torch.sigmoid(to["burst"]).numpy()
        p_t1 = to["t1"].numpy()
        p_t3 = to["t3"].numpy()
        p_oh = torch.sigmoid(to["oh"]).numpy()
        p_act = to["act"].argmax(dim=-1).numpy()

    # Latency benchmark
    with torch.no_grad():
        t0 = time.perf_counter()
        for i in range(200):
            _ = net(X_te_t[i : i + 1])
        cpu_ms = (time.perf_counter() - t0) * 1000.0 / 200.0

    n_params = sum(p.numel() for p in net.parameters())
    metrics = {
        "params": int(n_params),
        "weights_fp32_kb": round(n_params * 4 / 1024.0, 2),
        "distill_alpha": best_alpha,
        "distill_temp": best_temp,
        "cpu_latency_ms": round(cpu_ms, 4),
        "th_burst": round(th_b, 3),
        "th_overheat": round(th_o, 3),
        "cpu_next": reg_metrics(ds.y_cpu_next[te], p_cpu),
        "burst": bin_metrics(ds.y_burst[te], p_burst, th_b),
        "t1": reg_metrics(ds.y_t1[te], p_t1),
        "t3": reg_metrics(ds.y_t3[te], p_t3),
        "overheat_3m": bin_metrics(ds.y_overheat_3m[te], p_oh, th_o),
        "action": {
            "acc": round(float(accuracy_score(ds.y_action[te], p_act)), 4),
            "macro_f1": round(float(f1_score(ds.y_action[te], p_act, average="macro", zero_division=0)), 4),
            "weighted_f1": round(float(f1_score(ds.y_action[te], p_act, average="weighted", zero_division=0)), 4),
        },
    }

    with open(RES_DIR / "neural_results.json", encoding="utf-8") as f:
        nr = json.load(f)
    vanilla = nr["TinyMLP"]

    print(
        f"[Vanilla TinyMLP]   act_acc={vanilla['action']['acc']:.4f} act_f1={vanilla['action']['macro_f1']:.4f} "
        f"t1_mae={vanilla['t1']['mae']:.4f} oh_auc={vanilla['overheat_3m']['roc_auc']:.4f}"
    )
    print(
        f"[Distilled TinyMLP] act_acc={metrics['action']['acc']:.4f} act_f1={metrics['action']['macro_f1']:.4f} "
        f"t1_mae={metrics['t1']['mae']:.4f} oh_auc={metrics['overheat_3m']['roc_auc']:.4f} (alpha={best_alpha}, T={best_temp})"
    )

    nr["TinyMLP_Vanilla"] = vanilla
    nr["TinyMLP_Distilled"] = metrics

    if metrics["action"]["macro_f1"] >= vanilla["action"]["macro_f1"] or metrics["overheat_3m"]["roc_auc"] >= vanilla["overheat_3m"]["roc_auc"]:
        w_path = export_compact_tiny_mlp(net, tab_mean, tab_std, ds.tab_feature_names, metrics, ds)
        kb = round(w_path.stat().st_size / 1024.0, 2)
        metrics["exported_json_kb"] = kb
        nr["TinyMLP"] = metrics
        torch.save(net.state_dict(), ART_DIR / "tiny_mlp.pt")
        print(f"Replaced tiny_mlp_weights.json with Distilled weights ({kb} KiB)")

    with open(RES_DIR / "neural_results.json", "w", encoding="utf-8") as f:
        json.dump(nr, f, indent=2, ensure_ascii=False)


if __name__ == "__main__":
    main()

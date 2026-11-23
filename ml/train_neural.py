#!/usr/bin/env python3
"""Family 3: PyTorch Multi-Task Neural Models (TCN, GRU, Mini-Transformer, TinyMLP) on RTX 4060."""

from __future__ import annotations

import json
import time
from pathlib import Path

import numpy as np
import torch
import torch.nn as nn
from sklearn.metrics import (
    accuracy_score,
    average_precision_score,
    f1_score,
    mean_absolute_error,
    mean_squared_error,
    roc_auc_score,
)
from torch.utils.data import DataLoader, TensorDataset

from dataset import ACTION_NAMES, SEQ_LEN, build_dataset

ROOT = Path(__file__).resolve().parent
RES_DIR = ROOT / "results"
ART_DIR = ROOT / "artifacts"
RES_DIR.mkdir(parents=True, exist_ok=True)
ART_DIR.mkdir(parents=True, exist_ok=True)


def set_seed(seed: int = 42) -> None:
    np.random.seed(seed)
    torch.manual_seed(seed)
    if torch.cuda.is_available():
        torch.cuda.manual_seed_all(seed)


class MultiTaskHead(nn.Module):
    def __init__(self, d_in: int) -> None:
        super().__init__()
        self.cpu_head = nn.Linear(d_in, 1)
        self.burst_head = nn.Linear(d_in, 1)
        self.t1_head = nn.Linear(d_in, 1)
        self.t3_head = nn.Linear(d_in, 1)
        self.oh_head = nn.Linear(d_in, 1)
        self.act_head = nn.Linear(d_in, len(ACTION_NAMES))

    def forward(self, h: torch.Tensor) -> dict[str, torch.Tensor]:
        return {
            "cpu": self.cpu_head(h).squeeze(-1),
            "burst": self.burst_head(h).squeeze(-1),
            "t1": self.t1_head(h).squeeze(-1),
            "t3": self.t3_head(h).squeeze(-1),
            "oh": self.oh_head(h).squeeze(-1),
            "act": self.act_head(h),
        }


class MultiTaskTCN(nn.Module):
    def __init__(self, f_in: int, hidden: int = 64) -> None:
        super().__init__()
        self.net = nn.Sequential(
            nn.Conv1d(f_in, hidden, kernel_size=3, padding=2, dilation=1),
            nn.GELU(),
            nn.Dropout(0.15),
            nn.Conv1d(hidden, hidden, kernel_size=3, padding=4, dilation=2),
            nn.GELU(),
            nn.Dropout(0.15),
            nn.Conv1d(hidden, hidden, kernel_size=3, padding=8, dilation=4),
            nn.GELU(),
        )
        self.norm = nn.LayerNorm(hidden * 2)
        self.head = MultiTaskHead(hidden * 2)

    def forward(self, x: torch.Tensor) -> dict[str, torch.Tensor]:
        # x: (B, T, F)
        z = self.net(x.transpose(1, 2))[:, :, : x.size(1)]
        last = z[:, :, -1]
        avg = z.mean(dim=-1)
        h = self.norm(torch.cat([last, avg], dim=-1))
        return self.head(h)


class MultiTaskGRU(nn.Module):
    def __init__(self, f_in: int, hidden: int = 64) -> None:
        super().__init__()
        self.proj = nn.Sequential(nn.Linear(f_in, hidden), nn.GELU())
        self.gru = nn.GRU(hidden, hidden, num_layers=2, batch_first=True, dropout=0.15)
        self.norm = nn.LayerNorm(hidden * 2)
        self.head = MultiTaskHead(hidden * 2)

    def forward(self, x: torch.Tensor) -> dict[str, torch.Tensor]:
        z, _ = self.gru(self.proj(x))
        h = self.norm(torch.cat([z[:, -1, :], z.mean(dim=1)], dim=-1))
        return self.head(h)


class MultiTaskMiniTransformer(nn.Module):
    def __init__(self, f_in: int, d_model: int = 64, nhead: int = 4) -> None:
        super().__init__()
        self.proj = nn.Linear(f_in, d_model)
        self.pos = nn.Parameter(torch.randn(1, SEQ_LEN, d_model) * 0.02)
        layer = nn.TransformerEncoderLayer(
            d_model=d_model,
            nhead=nhead,
            dim_feedforward=128,
            dropout=0.15,
            activation="gelu",
            batch_first=True,
        )
        self.enc = nn.TransformerEncoder(layer, num_layers=2)
        self.norm = nn.LayerNorm(d_model * 2)
        self.head = MultiTaskHead(d_model * 2)

    def forward(self, x: torch.Tensor) -> dict[str, torch.Tensor]:
        z = self.enc(self.proj(x) + self.pos[:, : x.size(1), :])
        h = self.norm(torch.cat([z[:, -1, :], z.mean(dim=1)], dim=-1))
        return self.head(h)


class TinyMultiTaskMLP(nn.Module):
    """Ultra-compact MLP designed to be compiled directly into pure Rust (zero dependencies)."""

    def __init__(self, f_in: int, h1: int = 48, h2: int = 24) -> None:
        super().__init__()
        self.fc1 = nn.Linear(f_in, h1)
        self.fc2 = nn.Linear(h1, h2)
        self.act = nn.ReLU()
        self.head = MultiTaskHead(h2)

    def forward(self, x: torch.Tensor) -> dict[str, torch.Tensor]:
        h = self.act(self.fc1(x))
        h = self.act(self.fc2(h))
        return self.head(h)


def reg_metrics(y_true: np.ndarray, y_pred: np.ndarray) -> dict[str, float]:
    return {
        "mae": round(float(mean_absolute_error(y_true, y_pred)), 4),
        "rmse": round(float(np.sqrt(mean_squared_error(y_true, y_pred))), 4),
    }


def bin_metrics(y_true: np.ndarray, y_prob: np.ndarray, thresh: float) -> dict[str, float]:
    y_prob = np.clip(y_prob, 1e-6, 1.0 - 1e-6)
    pred = (y_prob >= thresh).astype(int)
    auc = float(roc_auc_score(y_true, y_prob)) if len(np.unique(y_true)) > 1 else 0.0
    ap = float(average_precision_score(y_true, y_prob)) if len(np.unique(y_true)) > 1 else 0.0
    return {
        "roc_auc": round(auc, 4),
        "pr_auc": round(ap, 4),
        "f1": round(float(f1_score(y_true, pred, zero_division=0)), 4),
        "acc": round(float(accuracy_score(y_true, pred)), 4),
    }


def tune_thresh(y_val: np.ndarray, p_val: np.ndarray) -> float:
    best_t, best_f1 = 0.5, -1.0
    for t in np.linspace(0.15, 0.85, 29):
        f1 = f1_score(y_val, (p_val >= t).astype(int), zero_division=0)
        if f1 > best_f1:
            best_f1 = f1
            best_t = float(t)
    return best_t


def train_and_eval_model(
    name: str,
    model: nn.Module,
    X_train: np.ndarray,
    X_val: np.ndarray,
    X_test: np.ndarray,
    ds,
    device: torch.device,
    epochs: int = 45,
    batch_size: int = 128,
    lr: float = 1.5e-3,
) -> tuple[dict, nn.Module]:
    tr, va, te = ds.train_idx, ds.val_idx, ds.test_idx
    model = model.to(device)

    # Class weights from train only
    b_pos = max(float((ds.y_burst[tr] == 1).sum()), 1.0)
    b_neg = max(float((ds.y_burst[tr] == 0).sum()), 1.0)
    o_pos = max(float((ds.y_overheat_3m[tr] == 1).sum()), 1.0)
    o_neg = max(float((ds.y_overheat_3m[tr] == 0).sum()), 1.0)

    act_counts = np.bincount(ds.y_action[tr], minlength=len(ACTION_NAMES)).astype(np.float32)
    act_w = act_counts.sum() / (len(ACTION_NAMES) * np.maximum(act_counts, 1.0))
    act_w = np.clip(act_w, 0.4, 6.0)

    bce_burst = nn.BCEWithLogitsLoss(pos_weight=torch.tensor([b_neg / b_pos], device=device))
    bce_oh = nn.BCEWithLogitsLoss(pos_weight=torch.tensor([o_neg / o_pos], device=device))
    ce_act = nn.CrossEntropyLoss(weight=torch.tensor(act_w, device=device))
    huber = nn.SmoothL1Loss(beta=2.0)

    # Normalize regression targets for stable multi-task balance (using train mean/std only)
    t_mean = float(ds.y_t1[tr].mean())
    t_std = max(float(ds.y_t1[tr].std()), 1.0)
    c_mean = float(ds.y_cpu_next[tr].mean())
    c_std = max(float(ds.y_cpu_next[tr].std()), 1.0)

    def make_loader(X, idx, shuffle):
        t_ds = TensorDataset(
            torch.tensor(X, dtype=torch.float32),
            torch.tensor(ds.y_cpu_next[idx], dtype=torch.float32),
            torch.tensor(ds.y_burst[idx], dtype=torch.float32),
            torch.tensor(ds.y_t1[idx], dtype=torch.float32),
            torch.tensor(ds.y_t3[idx], dtype=torch.float32),
            torch.tensor(ds.y_overheat_3m[idx], dtype=torch.float32),
            torch.tensor(ds.y_action[idx], dtype=torch.long),
        )
        return DataLoader(t_ds, batch_size=batch_size, shuffle=shuffle)

    tr_loader = make_loader(X_train, tr, True)
    opt = torch.optim.AdamW(model.parameters(), lr=lr, weight_decay=2e-4)
    sched = torch.optim.lr_scheduler.CosineAnnealingLR(opt, T_max=epochs)

    best_val_score = -1e9
    best_state = None
    patience = 0

    X_val_t = torch.tensor(X_val, dtype=torch.float32, device=device)
    X_test_t = torch.tensor(X_test, dtype=torch.float32, device=device)

    t_train_start = time.perf_counter()
    for ep in range(epochs):
        model.train()
        for xb, y_c, y_b, y_t1, y_t3, y_o, y_a in tr_loader:
            xb = xb.to(device)
            y_c, y_b = y_c.to(device), y_b.to(device)
            y_t1, y_t3, y_o, y_a = y_t1.to(device), y_t3.to(device), y_o.to(device), y_a.to(device)

            out = model(xb)
            loss = (
                huber((out["cpu"] - c_mean) / c_std, (y_c - c_mean) / c_std)
                + 0.9 * bce_burst(out["burst"], y_b)
                + huber((out["t1"] - t_mean) / t_std, (y_t1 - t_mean) / t_std)
                + huber((out["t3"] - t_mean) / t_std, (y_t3 - t_mean) / t_std)
                + 0.9 * bce_oh(out["oh"], y_o)
                + 1.3 * ce_act(out["act"], y_a)
            )
            opt.zero_grad()
            loss.backward()
            torch.nn.utils.clip_grad_norm_(model.parameters(), 2.0)
            opt.step()
        sched.step()

        model.eval()
        with torch.no_grad():
            vo = model(X_val_t)
            val_act = vo["act"].argmax(dim=-1).cpu().numpy()
            val_f1 = f1_score(ds.y_action[va], val_act, average="macro", zero_division=0)
            val_oh_p = torch.sigmoid(vo["oh"]).cpu().numpy()
            val_oh_auc = roc_auc_score(ds.y_overheat_3m[va], val_oh_p) if len(np.unique(ds.y_overheat_3m[va])) > 1 else 0.5
            val_t1_mae = mean_absolute_error(ds.y_t1[va], vo["t1"].cpu().numpy())
            score = val_f1 + 0.5 * val_oh_auc - 0.03 * val_t1_mae
        if score > best_val_score:
            best_val_score = score
            best_state = {k: v.detach().cpu().clone() for k, v in model.state_dict().items()}
            patience = 0
        else:
            patience += 1
            if patience >= 10:
                break

    train_s = time.perf_counter() - t_train_start
    if best_state is not None:
        model.load_state_dict(best_state)
    model.eval()

    with torch.no_grad():
        vo = model(X_val_t)
        th_b = tune_thresh(ds.y_burst[va], torch.sigmoid(vo["burst"]).cpu().numpy())
        th_o = tune_thresh(ds.y_overheat_3m[va], torch.sigmoid(vo["oh"]).cpu().numpy())

        to = model(X_test_t)
        p_cpu = np.clip(to["cpu"].cpu().numpy(), 0.0, 100.0)
        p_burst = torch.sigmoid(to["burst"]).cpu().numpy()
        p_t1 = to["t1"].cpu().numpy()
        p_t3 = to["t3"].cpu().numpy()
        p_oh = torch.sigmoid(to["oh"]).cpu().numpy()
        p_act = to["act"].argmax(dim=-1).cpu().numpy()

    # CPU single-sample latency check
    cpu_model = model.to("cpu").eval()
    sample_x = torch.tensor(X_test[:1], dtype=torch.float32)
    with torch.no_grad():
        for _ in range(10):
            _ = cpu_model(sample_x)
        t0 = time.perf_counter()
        for i in range(200):
            _ = cpu_model(torch.tensor(X_test[i : i + 1], dtype=torch.float32))
        cpu_ms = (time.perf_counter() - t0) * 1000.0 / 200.0

    n_params = sum(p.numel() for p in cpu_model.parameters())
    metrics = {
        "params": int(n_params),
        "weights_fp32_kb": round(n_params * 4 / 1024.0, 2),
        "train_sec": round(train_s, 2),
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
    return metrics, cpu_model


def export_tiny_mlp(
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

    # Pick 3 deterministic spread-out test samples for Rust numerical parity verification
    te = ds.test_idx
    sample_positions = [int(te[0]), int(te[len(te) // 2]), int(te[-1])]
    parity_samples = []
    model.eval()
    denom = np.maximum(std, 1e-4)
    with torch.no_grad():
        for idx in sample_positions:
            raw_f = ds.X_tab[idx].astype(np.float32)
            x_norm = ((raw_f - mean) / denom).astype(np.float32)
            out = model(torch.tensor(x_norm[None, :], dtype=torch.float32))
            probs = torch.softmax(out["act"][0], dim=-1).cpu().numpy()
            parity_samples.append({
                "raw_features": np.round(raw_f, 6).tolist(),
                "expected_cpu": round(float(np.clip(out["cpu"][0].item(), 0.0, 100.0)), 6),
                "expected_burst_prob": round(float(torch.sigmoid(out["burst"][0]).item()), 6),
                "expected_t1": round(float(out["t1"][0].item()), 6),
                "expected_t3": round(float(out["t3"][0].item()), 6),
                "expected_oh_prob": round(float(torch.sigmoid(out["oh"][0]).item()), 6),
                "expected_action_probs": [round(float(v), 6) for v in probs],
            })

    def arr(t: torch.Tensor) -> list:
        return np.round(t.detach().cpu().numpy(), 6).tolist()

    payload = {
        "schema": 1,
        "architecture": "TinyMultiTaskMLP",
        "feature_names": feat_names,
        "scaler_mean": np.round(mean, 6).tolist(),
        "scaler_std": np.round(std, 6).tolist(),
        "global_priors": global_priors,
        "action_names": ACTION_NAMES,
        "th_burst": metrics["th_burst"],
        "th_overheat": metrics["th_overheat"],
        "layers": {
            "fc1_w": arr(sd["fc1.weight"]),
            "fc1_b": arr(sd["fc1.bias"]),
            "fc2_w": arr(sd["fc2.weight"]),
            "fc2_b": arr(sd["fc2.bias"]),
            "cpu_w": arr(sd["head.cpu_head.weight"][0]),
            "cpu_b": float(round(sd["head.cpu_head.bias"][0].item(), 6)),
            "burst_w": arr(sd["head.burst_head.weight"][0]),
            "burst_b": float(round(sd["head.burst_head.bias"][0].item(), 6)),
            "t1_w": arr(sd["head.t1_head.weight"][0]),
            "t1_b": float(round(sd["head.t1_head.bias"][0].item(), 6)),
            "t3_w": arr(sd["head.t3_head.weight"][0]),
            "t3_b": float(round(sd["head.t3_head.bias"][0].item(), 6)),
            "oh_w": arr(sd["head.oh_head.weight"][0]),
            "oh_b": float(round(sd["head.oh_head.bias"][0].item(), 6)),
            "act_w": arr(sd["head.act_head.weight"]),
            "act_b": arr(sd["head.act_head.bias"]),
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
    device = torch.device("cuda" if torch.cuda.is_available() else "cpu")
    print(f"Training PyTorch models on device: {device}" + (f" ({torch.cuda.get_device_name(0)})" if device.type == "cuda" else ""))

    ds = build_dataset()
    tr, va, te = ds.train_idx, ds.val_idx, ds.test_idx

    # Standardize X_seq using train-only stats
    seq_mean = ds.X_seq[tr].mean(axis=(0, 1))
    seq_std = np.maximum(ds.X_seq[tr].std(axis=(0, 1)), 1e-4)
    X_seq_tr = (ds.X_seq[tr] - seq_mean) / seq_std
    X_seq_va = (ds.X_seq[va] - seq_mean) / seq_std
    X_seq_te = (ds.X_seq[te] - seq_mean) / seq_std

    # Standardize X_tab using train-only stats
    tab_mean = ds.X_tab[tr].mean(axis=0)
    tab_std = np.maximum(ds.X_tab[tr].std(axis=0), 1e-4)
    X_tab_tr = (ds.X_tab[tr] - tab_mean) / tab_std
    X_tab_va = (ds.X_tab[va] - tab_mean) / tab_std
    X_tab_te = (ds.X_tab[te] - tab_mean) / tab_std

    f_seq = ds.X_seq.shape[2]
    f_tab = ds.X_tab.shape[1]

    results = {"device": str(device)}
    models = [
        ("TCN", MultiTaskTCN(f_seq, hidden=64), X_seq_tr, X_seq_va, X_seq_te),
        ("GRU", MultiTaskGRU(f_seq, hidden=64), X_seq_tr, X_seq_va, X_seq_te),
        ("MiniTransformer", MultiTaskMiniTransformer(f_seq, d_model=64, nhead=4), X_seq_tr, X_seq_va, X_seq_te),
        ("TinyMLP", TinyMultiTaskMLP(f_tab, h1=48, h2=24), X_tab_tr, X_tab_va, X_tab_te),
    ]

    for name, net, xtr, xva, xte in models:
        metrics, trained_net = train_and_eval_model(
            name, net, xtr, xva, xte, ds, device=device, epochs=50
        )
        results[name] = metrics
        print(f"[{name}] params={metrics['params']} cpu_mae={metrics['cpu_next']['mae']} t1_mae={metrics['t1']['mae']} oh_auc={metrics['overheat_3m']['roc_auc']} act_acc={metrics['action']['acc']} act_f1={metrics['action']['macro_f1']}")
        if name == "TinyMLP":
            w_path = export_tiny_mlp(trained_net, tab_mean, tab_std, ds.tab_feature_names, metrics, ds)
            results[name]["exported_json_kb"] = round(w_path.stat().st_size / 1024.0, 2)
            torch.save(trained_net.state_dict(), ART_DIR / "tiny_mlp.pt")

    with open(RES_DIR / "neural_results.json", "w", encoding="utf-8") as f:
        json.dump(results, f, indent=2, ensure_ascii=False)


if __name__ == "__main__":
    main()

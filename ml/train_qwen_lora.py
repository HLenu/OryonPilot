#!/usr/bin/env python3
"""Family 4: Real Qwen2.5-0.5B-Instruct Zero-Shot & LoRA Fine-Tuning on RTX 4060."""

from __future__ import annotations

import argparse
import json
import os
import re
import time
from pathlib import Path

import numpy as np
import torch
from peft import LoraConfig, get_peft_model
from sklearn.metrics import (
    accuracy_score,
    f1_score,
    mean_absolute_error,
    mean_squared_error,
)
from torch.utils.data import DataLoader, Dataset
from transformers import AutoModelForCausalLM, AutoTokenizer

from dataset import DatasetBundle, build_dataset

ROOT = Path(__file__).resolve().parent
RES_DIR = ROOT / "results"
ART_DIR = ROOT / "artifacts" / "qwen25_05b_lora"
CACHE_DIR = ROOT / "cache" / "qwen25-05b"
RES_DIR.mkdir(parents=True, exist_ok=True)
ART_DIR.mkdir(parents=True, exist_ok=True)
CACHE_DIR.mkdir(parents=True, exist_ok=True)

MODEL_ID = "Qwen/Qwen2.5-0.5B-Instruct"
SYS_PROMPT = (
    "You are an SM8750 mobile scheduler. Given recent 5-minute telemetry and app profile, "
    "predict next-min CPU%, burst(0/1), CPU temp at +1m/+3m, overheat_3m(0/1), and action(0..4). "
    "Reply ONLY with valid compact JSON."
)


def download_model() -> Path:
    cfg_file = CACHE_DIR / "config.json"
    if cfg_file.exists():
        return CACHE_DIR

    os.environ.setdefault("HF_ENDPOINT", "https://hf-mirror.com")
    try:
        from huggingface_hub import snapshot_download

        print(f"Downloading {MODEL_ID} via {os.environ.get('HF_ENDPOINT')} ...")
        snapshot_download(
            repo_id=MODEL_ID,
            local_dir=str(CACHE_DIR),
        )
        if cfg_file.exists():
            return CACHE_DIR
    except Exception as exc:
        print(f"hf-mirror download failed ({exc}); trying modelscope...")

    from modelscope.hub.snapshot_download import snapshot_download as ms_download

    ms_download(MODEL_ID, local_dir=str(CACHE_DIR))
    return CACHE_DIR


def format_prompt(ds: DatasetBundle, idx: int) -> str:
    row = ds.df_valid.iloc[idx]
    seq = ds.X_seq[idx]  # (15, 22)
    # Last 5 steps (t-4 .. t)
    # seq cols: 0:cpu_pct, 1:prime_mhz_norm, 2:mem_avail_pct, 5:psi_some10, 8:cpu_temp_c, 9:batt_temp_c, 11:batt_pct_norm
    traj = []
    for t in range(seq.shape[0] - 5, seq.shape[0]):
        traj.append(
            f"[{seq[t,0]:.0f},{seq[t,1]:.1f},{seq[t,2]:.0f},{seq[t,5]:.1f},{seq[t,8]:.1f}]"
        )
    traj_str = "->".join(traj)
    chg = int(row["is_charging"] > 0.5)
    lnch = int(row["is_launcher"] > 0.5)
    return (
        f"T(-4..0)[cpu%,primeG,mem%,psi,Tcpu]: {traj_str}\n"
        f"Now: Tcpu={row['cpu_temp_c']:.1f} dT={row['d_temp_1m']:+.1f} "
        f"Tbatt={row['batt_temp_c']:.1f} bat={row['batt_pct']:.0f}% chg={chg} "
        f"psiF={row['psi_full10']:.1f} ksw={row['kswapd_pct']:.0f} "
        f"hr={int(row['ts_local'].hour)} app={row['anon_fg']}(lnch={lnch},"
        f"avgC={row['app_cpu_mean']:.0f},p90={row['app_cpu_p90']:.0f},avgT={row['app_temp_mean']:.1f},stay={int(row['fg_run_len'])})"
    )


def format_target(ds: DatasetBundle, idx: int) -> str:
    obj = {
        "cpu": int(round(float(ds.y_cpu_next[idx]))),
        "burst": int(ds.y_burst[idx]),
        "t1": round(float(ds.y_t1[idx]), 1),
        "t3": round(float(ds.y_t3[idx]), 1),
        "overheat": int(ds.y_overheat_3m[idx]),
        "act": int(ds.y_action[idx]),
    }
    return json.dumps(obj, separators=(",", ":"))


class SchedSFTDataset(Dataset):
    def __init__(self, ds: DatasetBundle, indices: np.ndarray, tokenizer, max_len: int = 320) -> None:
        self.samples = []
        for idx in indices:
            messages = [
                {"role": "system", "content": SYS_PROMPT},
                {"role": "user", "content": format_prompt(ds, int(idx))},
            ]
            prompt_text = tokenizer.apply_chat_template(
                messages, tokenize=False, add_generation_prompt=True
            )
            full_text = prompt_text + format_target(ds, int(idx)) + tokenizer.eos_token
            p_ids = tokenizer(prompt_text, add_special_tokens=False)["input_ids"]
            f_ids = tokenizer(
                full_text, add_special_tokens=False, truncation=True, max_length=max_len
            )["input_ids"]
            labels = [-100] * min(len(p_ids), len(f_ids)) + f_ids[len(p_ids) :]
            self.samples.append(
                (
                    torch.tensor(f_ids, dtype=torch.long),
                    torch.tensor(labels, dtype=torch.long),
                )
            )

    def __len__(self) -> int:
        return len(self.samples)

    def __getitem__(self, i: int):
        return self.samples[i]


def make_collate(pad_id: int):
    def collate(batch):
        max_l = max(x[0].size(0) for x in batch)
        input_ids = torch.full((len(batch), max_l), pad_id, dtype=torch.long)
        attn = torch.zeros((len(batch), max_l), dtype=torch.long)
        labels = torch.full((len(batch), max_l), -100, dtype=torch.long)
        for i, (ids, lbl) in enumerate(batch):
            l = ids.size(0)
            input_ids[i, :l] = ids
            attn[i, :l] = 1
            labels[i, :l] = lbl
        return {"input_ids": input_ids, "attention_mask": attn, "labels": labels}

    return collate


def parse_json_output(text: str) -> dict | None:
    m = re.search(r"\{[^{}]*\}", text)
    if not m:
        return None
    try:
        obj = json.loads(m.group(0))
        if not isinstance(obj, dict):
            return None
        for k in ("cpu", "burst", "t1", "t3", "overheat", "act"):
            if k not in obj:
                return None
        return {
            "cpu": float(np.clip(float(obj["cpu"]), 0.0, 100.0)),
            "burst": int(bool(int(round(float(obj["burst"]))))),
            "t1": float(np.clip(float(obj["t1"]), 10.0, 120.0)),
            "t3": float(np.clip(float(obj["t3"]), 10.0, 120.0)),
            "overheat": int(bool(int(round(float(obj["overheat"]))))),
            "act": int(np.clip(int(round(float(obj["act"]))), 0, 4)),
        }
    except Exception:
        return None


@torch.no_grad()
def evaluate_model(
    model,
    tokenizer,
    ds: DatasetBundle,
    indices: np.ndarray,
    device: torch.device,
    batch_size: int = 32,
) -> dict:
    model.eval()
    orig_padding = tokenizer.padding_side
    tokenizer.padding_side = "left"

    prompts = []
    for idx in indices:
        messages = [
            {"role": "system", "content": SYS_PROMPT},
            {"role": "user", "content": format_prompt(ds, int(idx))},
        ]
        prompts.append(
            tokenizer.apply_chat_template(messages, tokenize=False, add_generation_prompt=True)
        )

    parsed_preds = []
    t_start = time.perf_counter()
    for start in range(0, len(prompts), batch_size):
        batch_p = prompts[start : start + batch_size]
        enc = tokenizer(
            batch_p, return_tensors="pt", padding=True, truncation=True, max_length=320
        ).to(device)
        gen = model.generate(
            **enc,
            max_new_tokens=40,
            do_sample=False,
            pad_token_id=tokenizer.pad_token_id,
            eos_token_id=tokenizer.eos_token_id,
        )
        in_len = enc["input_ids"].size(1)
        for row in gen:
            txt = tokenizer.decode(row[in_len:], skip_special_tokens=True)
            parsed_preds.append(parse_json_output(txt))
    elapsed = time.perf_counter() - t_start
    tokenizer.padding_side = orig_padding

    valid_mask = np.array([p is not None for p in parsed_preds], dtype=bool)
    valid_rate = float(valid_mask.mean())

    # For unparsable outputs, fall back to neutral/persistence-safe default so metrics stay strictly aligned
    y_cpu_t = ds.y_cpu_next[indices]
    y_b_t = ds.y_burst[indices]
    y_t1_t = ds.y_t1[indices]
    y_t3_t = ds.y_t3[indices]
    y_oh_t = ds.y_overheat_3m[indices]
    y_act_t = ds.y_action[indices]

    p_cpu = np.array([p["cpu"] if p else 10.0 for p in parsed_preds])
    p_b = np.array([p["burst"] if p else 0 for p in parsed_preds])
    p_t1 = np.array([p["t1"] if p else 45.0 for p in parsed_preds])
    p_t3 = np.array([p["t3"] if p else 45.0 for p in parsed_preds])
    p_oh = np.array([p["overheat"] if p else 0 for p in parsed_preds])
    p_act = np.array([p["act"] if p else 1 for p in parsed_preds])

    return {
        "n_eval": int(len(indices)),
        "json_valid_rate": round(valid_rate, 4),
        "total_eval_sec": round(elapsed, 2),
        "ms_per_sample_gpu_batch": round(elapsed * 1000.0 / max(len(indices), 1), 2),
        "cpu_next": {
            "mae": round(float(mean_absolute_error(y_cpu_t, p_cpu)), 4),
            "rmse": round(float(np.sqrt(mean_squared_error(y_cpu_t, p_cpu))), 4),
        },
        "burst": {
            "f1": round(float(f1_score(y_b_t, p_b, zero_division=0)), 4),
            "acc": round(float(accuracy_score(y_b_t, p_b)), 4),
        },
        "t1": {
            "mae": round(float(mean_absolute_error(y_t1_t, p_t1)), 4),
            "rmse": round(float(np.sqrt(mean_squared_error(y_t1_t, p_t1))), 4),
        },
        "t3": {
            "mae": round(float(mean_absolute_error(y_t3_t, p_t3)), 4),
            "rmse": round(float(np.sqrt(mean_squared_error(y_t3_t, p_t3))), 4),
        },
        "overheat_3m": {
            "f1": round(float(f1_score(y_oh_t, p_oh, zero_division=0)), 4),
            "acc": round(float(accuracy_score(y_oh_t, p_oh)), 4),
        },
        "action": {
            "acc": round(float(accuracy_score(y_act_t, p_act)), 4),
            "macro_f1": round(float(f1_score(y_act_t, p_act, average="macro", zero_division=0)), 4),
            "weighted_f1": round(float(f1_score(y_act_t, p_act, average="weighted", zero_division=0)), 4),
        },
    }


def balanced_train_indices(ds: DatasetBundle, max_n: int = 4500, seed: int = 42) -> np.ndarray:
    rng = np.random.default_rng(seed)
    tr = ds.train_idx
    chosen = []
    # Keep all rare THERMAL_CAP & DEEP_SAVE & BOOST up to balanced budget
    per_cls = max_n // 5
    for cls in range(5):
        pool = tr[ds.y_action[tr] == cls]
        if len(pool) == 0:
            continue
        if len(pool) <= per_cls:
            # Mild oversample up to min(per_cls, 2 * len(pool))
            reps = min(per_cls, len(pool) * 2)
            sel = rng.choice(pool, size=reps, replace=True)
        else:
            sel = rng.choice(pool, size=per_cls, replace=False)
        chosen.append(sel)
    out = np.concatenate(chosen)
    rng.shuffle(out)
    return out


def main() -> None:
    parser = argparse.ArgumentParser(description="Qwen2.5-0.5B Zero-Shot & LoRA Scheduler Training")
    parser.add_argument("--download-only", action="store_true", help="Download model to cache and exit")
    parser.add_argument("--offline", action="store_true", help="Require cached local model and force HF_HUB_OFFLINE=1")
    args = parser.parse_args()

    if args.download_only:
        model_dir = download_model()
        print(f"Model cached cleanly at {model_dir}")
        return

    if args.offline:
        os.environ["HF_HUB_OFFLINE"] = "1"
        assert (CACHE_DIR / "config.json").exists(), f"Missing offline cached model at {CACHE_DIR}"
        model_dir = CACHE_DIR
    else:
        model_dir = download_model()

    torch.manual_seed(42)
    np.random.seed(42)
    device = torch.device("cuda" if torch.cuda.is_available() else "cpu")
    assert device.type == "cuda", "CUDA GPU is required for Qwen2.5-0.5B LoRA training"
    print(f"Qwen LoRA experiment on {torch.cuda.get_device_name(0)}")

    ds = build_dataset()

    tokenizer = AutoTokenizer.from_pretrained(str(model_dir), trust_remote_code=False)
    if tokenizer.pad_token_id is None:
        tokenizer.pad_token = tokenizer.eos_token

    dtype = torch.bfloat16 if torch.cuda.is_bf16_supported() else torch.float16
    try:
        base_model = AutoModelForCausalLM.from_pretrained(
            str(model_dir),
            dtype=dtype,
            device_map={"": device},
            trust_remote_code=False,
        )
    except TypeError:
        base_model = AutoModelForCausalLM.from_pretrained(
            str(model_dir),
            torch_dtype=dtype,
            device_map={"": device},
            trust_remote_code=False,
        )

    # Zero-Shot eval on stratified 250 test samples (fast & clean baseline)
    rng = np.random.default_rng(42)
    zs_idx = np.sort(rng.choice(ds.test_idx, size=min(250, len(ds.test_idx)), replace=False))
    print("Running Zero-Shot Qwen2.5-0.5B-Instruct on 250 test samples...")
    zs_metrics = evaluate_model(base_model, tokenizer, ds, zs_idx, device=device, batch_size=32)
    print("Zero-Shot metrics:", zs_metrics)

    # Configure LoRA
    base_model.config.use_cache = False
    base_model.enable_input_require_grads()
    base_model.gradient_checkpointing_enable()
    lora_cfg = LoraConfig(
        r=16,
        lora_alpha=32,
        lora_dropout=0.05,
        bias="none",
        task_type="CAUSAL_LM",
        target_modules=["q_proj", "k_proj", "v_proj", "o_proj", "gate_proj", "up_proj", "down_proj"],
    )
    model = get_peft_model(base_model, lora_cfg)
    model.print_trainable_parameters()

    tr_sub = balanced_train_indices(ds, max_n=4000, seed=42)
    va_sub = np.sort(rng.choice(ds.val_idx, size=min(240, len(ds.val_idx)), replace=False))
    print(f"Building SFT tokenized datasets (train={len(tr_sub)}, val={len(va_sub)})...")
    train_sft = SchedSFTDataset(ds, tr_sub, tokenizer)
    train_loader = DataLoader(
        train_sft,
        batch_size=8,
        shuffle=True,
        collate_fn=make_collate(tokenizer.pad_token_id),
    )

    epochs = 3
    grad_accum = 2
    opt = torch.optim.AdamW(model.parameters(), lr=2e-4, weight_decay=0.01)
    total_steps = (len(train_loader) // grad_accum) * epochs
    sched = torch.optim.lr_scheduler.CosineAnnealingLR(opt, T_max=max(total_steps, 1))

    torch.cuda.reset_peak_memory_stats(device)
    t_train = time.perf_counter()
    best_score = -1.0
    best_state = None
    train_losses = []
    for ep in range(epochs):
        model.train()
        opt.zero_grad()
        running = 0.0
        for step, batch in enumerate(train_loader, 1):
            batch = {k: v.to(device) for k, v in batch.items()}
            out = model(**batch)
            loss = out.loss / grad_accum
            loss.backward()
            running += float(out.loss.item())
            if step % grad_accum == 0 or step == len(train_loader):
                torch.nn.utils.clip_grad_norm_(model.parameters(), 1.0)
                opt.step()
                sched.step()
                opt.zero_grad()
        avg_loss = running / max(len(train_loader), 1)
        train_losses.append(round(avg_loss, 4))
        model.config.use_cache = True
        val_m = evaluate_model(model, tokenizer, ds, va_sub, device=device, batch_size=32)
        model.config.use_cache = False
        print(f"[Epoch {ep+1}/{epochs}] loss={avg_loss:.4f} val_act_acc={val_m['action']['acc']} val_act_f1={val_m['action']['macro_f1']} val_json={val_m['json_valid_rate']}")
        score = val_m["action"]["macro_f1"] + 0.1 * val_m["json_valid_rate"]
        if score >= best_score:
            best_score = score
            best_state = {k: v.detach().cpu().clone() for k, v in model.state_dict().items() if "lora_" in k}
            model.save_pretrained(str(ART_DIR))
            tokenizer.save_pretrained(str(ART_DIR))

    train_sec = time.perf_counter() - t_train
    peak_vram_mb = torch.cuda.max_memory_allocated(device) / (1024.0 * 1024.0)

    if best_state is not None:
        model.load_state_dict(best_state, strict=False)
    model.config.use_cache = True

    # Full blind test evaluation!
    print(f"Evaluating LoRA Qwen2.5-0.5B on FULL blind test set ({len(ds.test_idx)} samples)...")
    ft_full_metrics = evaluate_model(model, tokenizer, ds, ds.test_idx, device=device, batch_size=32)
    ft_250_metrics = evaluate_model(model, tokenizer, ds, zs_idx, device=device, batch_size=32)

    trainable_params = sum(p.numel() for p in model.parameters() if p.requires_grad)
    total_params = sum(p.numel() for p in model.parameters())
    adapter_kb = sum(p.stat().st_size for p in ART_DIR.glob("*") if p.is_file()) / 1024.0

    results = {
        "model_id": MODEL_ID,
        "gpu": torch.cuda.get_device_name(0),
        "dtype": str(dtype),
        "total_params": int(total_params),
        "trainable_lora_params": int(trainable_params),
        "adapter_disk_kb": round(adapter_kb, 1),
        "train_samples": int(len(tr_sub)),
        "epochs": epochs,
        "train_losses": train_losses,
        "train_sec": round(train_sec, 2),
        "peak_vram_mb": round(peak_vram_mb, 1),
        "zero_shot_250": zs_metrics,
        "lora_finetuned_250": ft_250_metrics,
        "lora_finetuned_full_test": ft_full_metrics,
    }
    with open(RES_DIR / "qwen_lora_results.json", "w", encoding="utf-8") as f:
        json.dump(results, f, indent=2, ensure_ascii=False)
    print("LoRA FULL test metrics:", ft_full_metrics)

    del model, base_model
    torch.cuda.empty_cache()


if __name__ == "__main__":
    main()

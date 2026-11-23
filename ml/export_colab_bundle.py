#!/usr/bin/env python3
"""Export strictly anonymized SFT dataset & self-contained Colab GPU notebook for Qwen2.5-0.5B LoRA."""

from __future__ import annotations

import json
import re
from pathlib import Path

import numpy as np

from dataset import LAUNCHERS, SYSTEM_APPS, build_dataset
from train_qwen_lora import (
    MODEL_ID,
    SYS_PROMPT,
    balanced_train_indices,
    format_prompt,
    format_target,
)

ROOT = Path(__file__).resolve().parent
COLAB_DIR = ROOT / "colab"
COLAB_DIR.mkdir(parents=True, exist_ok=True)

ALLOWED_PKGS = LAUNCHERS | SYSTEM_APPS
PKG_PATTERN = re.compile(r"\b[a-z][a-z0-9_]*\.[a-z][a-z0-9_]*\.[a-z0-9_.]+\b")


def assert_anonymized(records: list[dict], split_name: str) -> None:
    for i, rec in enumerate(records):
        for m in PKG_PATTERN.findall(rec["prompt"]):
            assert m in ALLOWED_PKGS, f"Privacy leak detected in {split_name}[{i}]: {m!r}"


def build_eval_record(ds, idx: int) -> dict:
    return {
        "prompt": format_prompt(ds, idx),
        "y": {
            "cpu": float(ds.y_cpu_next[idx]),
            "burst": int(ds.y_burst[idx]),
            "t1": float(ds.y_t1[idx]),
            "t3": float(ds.y_t3[idx]),
            "overheat": int(ds.y_overheat_3m[idx]),
            "act": int(ds.y_action[idx]),
        },
    }


def make_code_cell(src: str) -> dict:
    lines = src.splitlines(keepends=True)
    return {
        "cell_type": "code",
        "execution_count": None,
        "metadata": {},
        "outputs": [],
        "source": lines,
    }


def make_md_cell(src: str) -> dict:
    lines = src.splitlines(keepends=True)
    return {
        "cell_type": "markdown",
        "metadata": {},
        "source": lines,
    }


def build_notebook() -> dict:
    c_md = """# OryonPilot — Qwen2.5-0.5B Zero-Shot & LoRA Scheduler Training (SM8750 Anonymized Telemetry)
Trains `Qwen/Qwen2.5-0.5B-Instruct` via LoRA (`r=16, alpha=32`) on 17 days of Xiaomi 15 telemetry and evaluates Zero-Shot vs LoRA on the identical blind test split.
"""

    c1 = """# 1. Verify GPU runtime & install missing PEFT / Accelerate dependencies
import importlib, subprocess, sys
!nvidia-smi

missing = []
for pkg in ("peft", "accelerate", "transformers", "sklearn"):
    try:
        importlib.import_module(pkg)
    except ImportError:
        missing.append("scikit-learn" if pkg == "sklearn" else pkg)

if missing:
    print("Installing:", missing)
    subprocess.check_call([sys.executable, "-m", "pip", "install", "-q", *missing])

import torch, transformers, peft
print("torch:", torch.__version__, "| cuda:", torch.cuda.is_available(), "| gpu:", torch.cuda.get_device_name(0) if torch.cuda.is_available() else "CPU")
print("transformers:", transformers.__version__, "| peft:", peft.__version__)
assert torch.cuda.is_available(), "Please switch Colab Runtime -> Change runtime type -> GPU!"
"""

    c2 = """# 2. Load strictly anonymized SFT dataset bundle
import json
from pathlib import Path

data_path = Path("/content/oryon_sft_data.json")
if not data_path.exists():
    cands = sorted(Path("/content").glob("*oryon_sft_data*.json"))
    if cands:
        data_path = cands[0]
    else:
        from google.colab import files
        print("Upload oryon_sft_data.json ...")
        uploaded = files.upload()
        data_path = Path(next(iter(uploaded.keys())))

with open(data_path, encoding="utf-8") as f:
    BUNDLE = json.load(f)

SYS_PROMPT = BUNDLE["sys_prompt"]
print(
    f"Loaded {data_path}: train={len(BUNDLE['train'])}, val={len(BUNDLE['val'])}, "
    f"test_250={len(BUNDLE['test_250'])}, test_full={len(BUNDLE['test_full'])}"
)
"""

    c3 = """# 3. Load Qwen2.5-0.5B-Instruct & define SFT dataset + blind evaluator
import os, re, time, json
import numpy as np
import torch
from torch.utils.data import Dataset, DataLoader
from transformers import AutoTokenizer, AutoModelForCausalLM
from peft import LoraConfig, get_peft_model
from sklearn.metrics import accuracy_score, f1_score, mean_absolute_error, mean_squared_error

MODEL_ID = BUNDLE.get("model_id", "Qwen/Qwen2.5-0.5B-Instruct")
device = torch.device("cuda")
dtype = torch.bfloat16 if torch.cuda.is_bf16_supported() else torch.float16
print(f"Using model={MODEL_ID}, dtype={dtype}")

tokenizer = AutoTokenizer.from_pretrained(MODEL_ID)
if tokenizer.pad_token_id is None:
    tokenizer.pad_token = tokenizer.eos_token

try:
    base_model = AutoModelForCausalLM.from_pretrained(MODEL_ID, dtype=dtype, device_map="auto")
except TypeError:
    base_model = AutoModelForCausalLM.from_pretrained(MODEL_ID, torch_dtype=dtype, device_map="auto")


class SchedSFTDataset(Dataset):
    def __init__(self, records, tok, max_len=320):
        self.samples = []
        for rec in records:
            messages = [
                {"role": "system", "content": SYS_PROMPT},
                {"role": "user", "content": rec["prompt"]},
            ]
            p_txt = tok.apply_chat_template(messages, tokenize=False, add_generation_prompt=True)
            f_txt = p_txt + rec["target"] + tok.eos_token
            p_ids = tok(p_txt, add_special_tokens=False)["input_ids"]
            f_ids = tok(f_txt, add_special_tokens=False, truncation=True, max_length=max_len)["input_ids"]
            labels = [-100] * min(len(p_ids), len(f_ids)) + f_ids[len(p_ids):]
            self.samples.append((torch.tensor(f_ids, dtype=torch.long), torch.tensor(labels, dtype=torch.long)))

    def __len__(self):
        return len(self.samples)

    def __getitem__(self, i):
        return self.samples[i]


def make_collate(pad_id):
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


def parse_json_output(text):
    m = re.search(r"\\{[^{}]*\\}", text)
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
def evaluate_records(model, tok, records, batch_size=64):
    model.eval()
    orig_padding = tok.padding_side
    tok.padding_side = "left"
    prompts = [
        tok.apply_chat_template(
            [{"role": "system", "content": SYS_PROMPT}, {"role": "user", "content": r["prompt"]}],
            tokenize=False,
            add_generation_prompt=True,
        )
        for r in records
    ]
    parsed = []
    t0 = time.perf_counter()
    for start in range(0, len(prompts), batch_size):
        bp = prompts[start:start + batch_size]
        enc = tok(bp, return_tensors="pt", padding=True, truncation=True, max_length=320).to(device)
        gen = model.generate(
            **enc,
            max_new_tokens=40,
            do_sample=False,
            pad_token_id=tok.pad_token_id,
            eos_token_id=tok.eos_token_id,
        )
        in_len = enc["input_ids"].size(1)
        for row in gen:
            parsed.append(parse_json_output(tok.decode(row[in_len:], skip_special_tokens=True)))
    elapsed = time.perf_counter() - t0
    tok.padding_side = orig_padding

    valid_rate = float(np.mean([p is not None for p in parsed]))
    y_cpu = np.array([r["y"]["cpu"] for r in records])
    y_b = np.array([r["y"]["burst"] for r in records])
    y_t1 = np.array([r["y"]["t1"] for r in records])
    y_t3 = np.array([r["y"]["t3"] for r in records])
    y_oh = np.array([r["y"]["overheat"] for r in records])
    y_act = np.array([r["y"]["act"] for r in records])

    p_cpu = np.array([p["cpu"] if p else 10.0 for p in parsed])
    p_b = np.array([p["burst"] if p else 0 for p in parsed])
    p_t1 = np.array([p["t1"] if p else 45.0 for p in parsed])
    p_t3 = np.array([p["t3"] if p else 45.0 for p in parsed])
    p_oh = np.array([p["overheat"] if p else 0 for p in parsed])
    p_act = np.array([p["act"] if p else 1 for p in parsed])

    return {
        "n_eval": int(len(records)),
        "json_valid_rate": round(valid_rate, 4),
        "total_eval_sec": round(elapsed, 2),
        "ms_per_sample_gpu_batch": round(elapsed * 1000.0 / max(len(records), 1), 2),
        "cpu_next": {"mae": round(float(mean_absolute_error(y_cpu, p_cpu)), 4), "rmse": round(float(np.sqrt(mean_squared_error(y_cpu, p_cpu))), 4)},
        "burst": {"f1": round(float(f1_score(y_b, p_b, zero_division=0)), 4), "acc": round(float(accuracy_score(y_b, p_b)), 4)},
        "t1": {"mae": round(float(mean_absolute_error(y_t1, p_t1)), 4), "rmse": round(float(np.sqrt(mean_squared_error(y_t1, p_t1))), 4)},
        "t3": {"mae": round(float(mean_absolute_error(y_t3, p_t3)), 4), "rmse": round(float(np.sqrt(mean_squared_error(y_t3, p_t3))), 4)},
        "overheat_3m": {"f1": round(float(f1_score(y_oh, p_oh, zero_division=0)), 4), "acc": round(float(accuracy_score(y_oh, p_oh)), 4)},
        "action": {
            "acc": round(float(accuracy_score(y_act, p_act)), 4),
            "macro_f1": round(float(f1_score(y_act, p_act, average="macro", zero_division=0)), 4),
            "weighted_f1": round(float(f1_score(y_act, p_act, average="weighted", zero_division=0)), 4),
        },
    }
"""

    c4 = """# 4. Evaluate Zero-Shot Qwen2.5-0.5B-Instruct on blind test_250
print("Running Zero-Shot Qwen2.5-0.5B-Instruct on test_250...")
zs_metrics = evaluate_records(base_model, tokenizer, BUNDLE["test_250"], batch_size=64)
print("Zero-Shot metrics:", json.dumps(zs_metrics, indent=2))
"""

    c5 = """# 5. LoRA Fine-Tuning (3 epochs, r=16, alpha=32) + validation checkpointing
torch.manual_seed(42)
np.random.seed(42)

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

train_sft = SchedSFTDataset(BUNDLE["train"], tokenizer, max_len=320)
train_loader = DataLoader(train_sft, batch_size=8, shuffle=True, collate_fn=make_collate(tokenizer.pad_token_id))

epochs = 3
grad_accum = 2
opt = torch.optim.AdamW(model.parameters(), lr=2e-4, weight_decay=0.01)
total_steps = (len(train_loader) // grad_accum) * epochs
sched = torch.optim.lr_scheduler.CosineAnnealingLR(opt, T_max=max(total_steps, 1))

adapter_dir = Path("/content/qwen25_05b_lora")
adapter_dir.mkdir(parents=True, exist_ok=True)
torch.cuda.reset_peak_memory_stats(device)
t_train = time.perf_counter()
best_score = -1.0
train_losses = []
best_state = None

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
    val_m = evaluate_records(model, tokenizer, BUNDLE["val"], batch_size=64)
    model.config.use_cache = False
    print(f"[Epoch {ep+1}/{epochs}] loss={avg_loss:.4f} val_acc={val_m['action']['acc']} val_f1={val_m['action']['macro_f1']} val_json={val_m['json_valid_rate']}")
    score = val_m["action"]["macro_f1"] + 0.1 * val_m["json_valid_rate"]
    if score >= best_score:
        best_score = score
        best_state = {k: v.detach().cpu().clone() for k, v in model.state_dict().items() if "lora_" in k}
        model.save_pretrained(str(adapter_dir))
        tokenizer.save_pretrained(str(adapter_dir))

train_sec = time.perf_counter() - t_train
peak_vram_mb = torch.cuda.max_memory_allocated(device) / (1024.0 * 1024.0)
if best_state is not None:
    model.load_state_dict(best_state, strict=False)
model.config.use_cache = True
print(f"Training finished in {train_sec:.1f}s | peak_vram_mb={peak_vram_mb:.1f}")
"""

    c6 = """# 6. Full Blind Test Evaluation + Export qwen_lora_results.json
print(f"Evaluating LoRA Qwen2.5-0.5B on test_250 and FULL test ({len(BUNDLE['test_full'])} samples)...")
ft_250 = evaluate_records(model, tokenizer, BUNDLE["test_250"], batch_size=64)
ft_full = evaluate_records(model, tokenizer, BUNDLE["test_full"], batch_size=64)

trainable_params = sum(p.numel() for p in model.parameters() if p.requires_grad)
total_params = sum(p.numel() for p in model.parameters())
adapter_kb = sum(p.stat().st_size for p in adapter_dir.glob("*") if p.is_file()) / 1024.0

results = {
    "model_id": MODEL_ID,
    "gpu": torch.cuda.get_device_name(0),
    "dtype": str(dtype),
    "total_params": int(total_params),
    "trainable_lora_params": int(trainable_params),
    "adapter_disk_kb": round(adapter_kb, 1),
    "train_samples": int(len(BUNDLE["train"])),
    "epochs": epochs,
    "train_losses": train_losses,
    "train_sec": round(train_sec, 2),
    "peak_vram_mb": round(peak_vram_mb, 1),
    "zero_shot_250": zs_metrics,
    "lora_finetuned_250": ft_250,
    "lora_finetuned_full_test": ft_full,
}

out_path = Path("/content/qwen_lora_results.json")
with open(out_path, "w", encoding="utf-8") as f:
    json.dump(results, f, indent=2, ensure_ascii=False)

print("===ORYON_QWEN_RESULTS_START===")
print(json.dumps(results, ensure_ascii=False))
print("===ORYON_QWEN_RESULTS_END===")

try:
    from google.colab import files
    files.download(str(out_path))
except Exception as exc:
    print("Auto-download skipped:", exc)
"""

    return {
        "nbformat": 4,
        "nbformat_minor": 5,
        "metadata": {
            "kernelspec": {
                "display_name": "Python 3",
                "language": "python",
                "name": "python3",
            },
            "language_info": {"name": "python"},
            "accelerator": "GPU",
        },
        "cells": [
            make_md_cell(c_md),
            make_code_cell(c1),
            make_code_cell(c2),
            make_code_cell(c3),
            make_code_cell(c4),
            make_code_cell(c5),
            make_code_cell(c6),
        ],
    }


def main() -> None:
    ds = build_dataset()
    rng = np.random.default_rng(42)

    tr_sub = balanced_train_indices(ds, max_n=4000, seed=42)
    va_sub = np.sort(rng.choice(ds.val_idx, size=min(240, len(ds.val_idx)), replace=False))
    zs_idx = np.sort(rng.choice(ds.test_idx, size=min(250, len(ds.test_idx)), replace=False))

    train_recs = [
        {"prompt": format_prompt(ds, int(i)), "target": format_target(ds, int(i))}
        for i in tr_sub
    ]
    val_recs = [build_eval_record(ds, int(i)) for i in va_sub]
    test_250_recs = [build_eval_record(ds, int(i)) for i in zs_idx]
    test_full_recs = [build_eval_record(ds, int(i)) for i in ds.test_idx]

    assert_anonymized(train_recs, "train")
    assert_anonymized(val_recs, "val")
    assert_anonymized(test_250_recs, "test_250")
    assert_anonymized(test_full_recs, "test_full")

    data_out = COLAB_DIR / "oryon_sft_data.json"
    with open(data_out, "w", encoding="utf-8") as f:
        json.dump(
            {
                "model_id": MODEL_ID,
                "sys_prompt": SYS_PROMPT,
                "train": train_recs,
                "val": val_recs,
                "test_250": test_250_recs,
                "test_full": test_full_recs,
            },
            f,
            ensure_ascii=False,
            separators=(",", ":"),
        )

    nb_out = COLAB_DIR / "OryonPilot_Qwen05B_LoRA.ipynb"
    with open(nb_out, "w", encoding="utf-8") as f:
        json.dump(build_notebook(), f, indent=2, ensure_ascii=False)

    print(
        f"Exported anonymized Colab bundle (privacy check PASSED): "
        f"{data_out.name}={data_out.stat().st_size / 1024.0:.1f} KiB, "
        f"{nb_out.name}={nb_out.stat().st_size / 1024.0:.1f} KiB"
    )


if __name__ == "__main__":
    main()

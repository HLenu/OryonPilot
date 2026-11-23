#!/usr/bin/env python3
"""Automated Offline Retraining & Regression Gate Pipeline for OryonPilot.

Workflow:
  1. Optional read-only pull from Xiaomi 15 telemetry (/data/adb/neurosched/telemetry/history.csv + model.json)
  2. EDA + Dataset construction (zero-leakage time-ordered Split)
  3. Train Tabular Baselines + Full LightGBM + CompactGBDT_FQI (<55 KiB JSON)
  4. Run Counterfactual SM8750 RL & Continuous Learning Simulation
  5. Compare candidate against currently embedded `daemon/src/learner/default_model.json`
     and emit PASS/FAIL deployment verdict. Never writes to phone automatically.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent
PY = sys.executable

DATA_DIR = ROOT / "data"
ART_DIR = ROOT / "artifacts"
RES_DIR = ROOT / "results"
EMBEDDED_MODEL = REPO / "daemon" / "src" / "learner" / "default_model.json"
CANDIDATE_MODEL = ART_DIR / "sched_model.json"


def run_step(script_name: str) -> None:
    cmd = [PY, "-X", "utf8", str(ROOT / script_name)]
    print(f"[pipeline] $ {' '.join(cmd)}")
    subprocess.run(cmd, cwd=str(REPO), check=True)


def pull_from_device(adb_serial: str) -> None:
    DATA_DIR.mkdir(parents=True, exist_ok=True)
    tmp_dir = "/data/local/tmp/oryon-pull"
    try:
        subprocess.run(
            [
                "adb",
                "-s",
                adb_serial,
                "shell",
                f"su -c 'mkdir -p {tmp_dir} && cp /data/adb/neurosched/telemetry/history.csv {tmp_dir}/history.csv && cp /data/adb/neurosched/model.json {tmp_dir}/model.json && chmod 644 {tmp_dir}/*'",
            ],
            check=True,
        )
        subprocess.run(
            ["adb", "-s", adb_serial, "pull", f"{tmp_dir}/history.csv", str(DATA_DIR / "history.csv")],
            check=True,
        )
        subprocess.run(
            ["adb", "-s", adb_serial, "pull", f"{tmp_dir}/model.json", str(DATA_DIR / "model.json")],
            check=True,
        )
    finally:
        subprocess.run(
            ["adb", "-s", adb_serial, "shell", f"rm -rf {tmp_dir}"],
            check=False,
        )


def evaluate_regression_gate() -> bool:
    cand = json.loads(CANDIDATE_MODEL.read_text(encoding="utf-8"))
    cm = cand["test_metrics"]

    if EMBEDDED_MODEL.exists():
        base = json.loads(EMBEDDED_MODEL.read_text(encoding="utf-8"))
        bm = base["test_metrics"]
    else:
        bm = cm

    rl = json.loads((RES_DIR / "rl_sim_results.json").read_text(encoding="utf-8"))
    test_rl = rl["blind_test_simulation"]
    full_rl = rl["full_17d_continuous_learning"]

    static_test_rew = test_rl["StaticRules_v012"]["mean_reward"]
    cand_test_rew = test_rl["WarmStart_FQI_Residual_LinUCB"]["mean_reward"]
    static_17d_rew = full_rl["StaticRules_v012"]["mean_reward"]
    cand_17d_rew = full_rl["WarmStart_FQI_Residual_LinUCB"]["mean_reward"]

    checks = [
        ("JSON size <= 55 KiB", cm["json_kb"] <= 55.0, f"{cm['json_kb']} KiB"),
        (
            "Tree parity < 1e-2",
            cm["max_tree_parity_err"] < 1e-2,
            f"{cm['max_tree_parity_err']}",
        ),
        (
            "Action Macro-F1 >= baseline - 0.015",
            cm["action"]["macro_f1"] >= bm["action"]["macro_f1"] - 0.015,
            f"{cm['action']['macro_f1']:.4f} vs {bm['action']['macro_f1']:.4f}",
        ),
        (
            "Overheat AUC >= baseline - 0.015",
            cm["overheat_3m"]["roc_auc"] >= bm["overheat_3m"]["roc_auc"] - 0.015,
            f"{cm['overheat_3m']['roc_auc']:.4f} vs {bm['overheat_3m']['roc_auc']:.4f}",
        ),
        (
            "T+1 MAE <= baseline + 0.20C",
            cm["t1"]["mae"] <= bm["t1"]["mae"] + 0.20,
            f"{cm['t1']['mae']:.4f}C vs {bm['t1']['mae']:.4f}C",
        ),
        (
            "RL Test Reward > StaticRules_v012",
            cand_test_rew > static_test_rew,
            f"{cand_test_rew:+.4f} vs {static_test_rew:+.4f}",
        ),
        (
            "RL 17d Reward > StaticRules_v012",
            cand_17d_rew > static_17d_rew,
            f"{cand_17d_rew:+.4f} vs {static_17d_rew:+.4f}",
        ),
    ]

    print("\n=== Regression Gate Report ===")
    all_ok = True
    for name, ok, detail in checks:
        mark = "PASS" if ok else "FAIL"
        print(f"  [{mark}] {name:38s} | {detail}")
        if not ok:
            all_ok = False
    return all_ok


def main() -> None:
    parser = argparse.ArgumentParser(description="OryonPilot continuous retraining pipeline")
    parser.add_argument("--pull", action="store_true", help="Pull latest telemetry via read-only ADB first")
    parser.add_argument("--skip-pull", action="store_true", help="Use existing ml/data/* without ADB")
    parser.add_argument("--adb-serial", default="192.168.77.213:43353", help="ADB device serial")
    parser.add_argument("--dry-run", action="store_true", help="Evaluate gate against current artifacts without retraining")
    args = parser.parse_args()

    if args.pull and not args.skip_pull and not args.dry_run:
        pull_from_device(args.adb_serial)

    if args.dry_run and CANDIDATE_MODEL.exists() and (RES_DIR / "rl_sim_results.json").exists():
        print("[pipeline] --dry-run: validating regression gate on existing artifacts/results...")
    else:
        run_step("eda.py")
        run_step("dataset.py")
        run_step("train_tabular.py")
        run_step("train_compact_gbdt.py")
        run_step("sim_rl.py")

    passed = evaluate_regression_gate()
    if not passed:
        raise SystemExit("Regression gate FAILED: candidate model rejected.")

    print(
        f"\nCandidate ready: {CANDIDATE_MODEL}\n"
        "Manual deploy (optional, requires user consent): "
        "adb push ml/artifacts/sched_model.json /data/adb/neurosched/sched_model.json"
    )


if __name__ == "__main__":
    main()

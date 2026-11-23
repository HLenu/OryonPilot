#!/usr/bin/env python3
"""Offline Counterfactual Environment, LinUCB Continuous Learning & Fitted Q-Iteration (RL) Simulation."""

from __future__ import annotations

import json
import pickle
from pathlib import Path

import numpy as np
from sklearn.linear_model import Ridge
from sklearn.preprocessing import StandardScaler

from dataset import ACTION_NAMES, ACTION_UCLAMP, build_dataset

ROOT = Path(__file__).resolve().parent
RES_DIR = ROOT / "results"
ART_DIR = ROOT / "artifacts"
RES_DIR.mkdir(parents=True, exist_ok=True)


def fit_thermal_env(ds) -> tuple[Ridge, float, float]:
    """Fit SM8750 counterfactual delta-T model using TRAIN set only.

    Models dT_{t+1} as a function of current T, T - T_batt, cpu%, prime_GHz, and
    effective uclamp cap/min so counterfactual policies realistically alter thermal trajectory.
    """
    tr = ds.train_idx
    df_tr = ds.df_valid.iloc[tr]
    T0 = df_tr["cpu_temp_c"].values
    Tb = df_tr["batt_temp_c"].values
    cpu = df_tr["cpu_pct"].values
    prime = df_tr["prime_mhz_norm"].values
    dT_obs = ds.y_t1[tr] - T0

    X_env = np.column_stack([T0, T0 - Tb, cpu, prime, np.ones_like(T0)])
    reg = Ridge(alpha=5.0, fit_intercept=False).fit(X_env, dT_obs)
    # Effective thermal sensitivity to uclamp max reduction / boost floor
    # Derived physically from observed prime_mhz sensitivity coef[3]
    prime_coef = max(float(reg.coef_[3]), 0.15)
    cpu_coef = max(float(reg.coef_[2]), 0.02)
    return reg, prime_coef, cpu_coef


def compute_step_reward(
    act: int,
    prev_act: int,
    cpu_next: float,
    prime_next: float,
    T_cf: float,
    T_batt: float,
    psi_full: float,
    mem_pct: float,
    is_launcher: float,
    is_charging: float,
    batt_pct: float,
) -> tuple[float, dict[str, float]]:
    """Multi-objective reward balancing responsiveness, thermal safety, idle power, and stability."""
    umin, umax = ACTION_UCLAMP[act]
    cap_ratio = umax / 1024.0
    floor_ratio = umin / 1024.0

    # Demand intensity proxy in [0, 1]
    demand = np.clip(max(cpu_next / 40.0, (prime_next - 1000.0) / 3000.0), 0.0, 1.2)
    if psi_full >= 3.0 or mem_pct < 8.0:
        demand = min(demand, 0.75)

    # 1. Performance / responsiveness utility vs under-provisioning penalty
    if cap_ratio < demand:
        # Capped below real demand -> lag penalty (unless thermally critical)
        shortfall = demand - cap_ratio
        perf_score = -2.2 * shortfall * (1.0 if T_cf < 75.0 else 0.35)
    else:
        # Headroom satisfaction + boost bonus during real bursts
        perf_score = 0.65 * min(demand, 1.0)
        if demand >= 0.55 and floor_ratio >= 0.25 and T_cf < 72.0:
            perf_score += 0.45
        elif demand >= 0.25 and floor_ratio >= 0.10 and T_cf < 75.0:
            perf_score += 0.20

    # 2. Thermal penalty (smooth rise above 72C, steep above 78C/83C)
    thermal_pen = 0.0
    if T_cf >= 72.0:
        thermal_pen += 0.09 * (T_cf - 72.0)
    if T_cf >= 78.0:
        thermal_pen += 0.22 * (T_cf - 78.0)
    if T_cf >= 85.0:
        thermal_pen += 0.45 * (T_cf - 85.0)

    # 3. Wasted power penalty (high uclamp floor or unrestricted cap during idle / launcher / low batt)
    waste_pen = 0.0
    if demand < 0.20:
        waste_pen += 0.85 * floor_ratio + 0.28 * max(0.0, cap_ratio - 0.50)
    elif demand < 0.40:
        waste_pen += 0.40 * max(0.0, floor_ratio - 0.125) + 0.12 * max(0.0, cap_ratio - 0.75)
    if is_launcher > 0.5 and demand < 0.25:
        waste_pen += 0.30 * floor_ratio + 0.15 * max(0.0, cap_ratio - 0.55)
    if is_charging < 0.5 and batt_pct <= 20.0:
        waste_pen += 0.25 * max(0.0, cap_ratio - 0.50)

    # 4. Action flapping penalty
    switch_pen = 0.08 if act != prev_act else 0.0

    total = perf_score - thermal_pen - waste_pen - switch_pen
    return float(total), {
        "perf": float(perf_score),
        "thermal_pen": float(thermal_pen),
        "waste_pen": float(waste_pen),
        "switch_pen": float(switch_pen),
    }


def simulate_policy(
    ds,
    indices: np.ndarray,
    policy_fn,
    env_reg: Ridge,
    prime_coef: float,
    online_update_fn=None,
) -> dict:
    df = ds.df_valid
    segs = df["seg_id"].values
    T_cf = float(df["cpu_temp_c"].iloc[indices[0]])
    prev_seg = int(segs[indices[0]])
    prev_act = 1

    rewards = []
    perfs = []
    therms = []
    wastes = []
    switches = 0
    overheat_mins = 0
    temps = []
    act_counts = [0] * len(ACTION_NAMES)

    for step_k, idx in enumerate(indices):
        cur_seg = int(segs[idx])
        T_obs = float(df["cpu_temp_c"].iloc[idx])
        if cur_seg != prev_seg:
            T_cf = T_obs
            prev_seg = cur_seg
            prev_act = 1
        else:
            # Anchor gently toward real ambient/workload trajectory while preserving thermal delta from policy
            T_cf = 0.35 * T_obs + 0.65 * T_cf

        act = int(policy_fn(step_k, idx, T_cf, prev_act))
        act_counts[act] += 1
        if act != prev_act:
            switches += 1

        umin, umax = ACTION_UCLAMP[act]
        # Counterfactual thermal shift caused by cap & boost floor relative to observed stock execution
        cap_ratio = umax / 1024.0
        floor_ratio = umin / 1024.0
        prime_obs = float(df["prime_mhz_norm"].iloc[idx])
        cpu_obs = float(df["cpu_pct"].iloc[idx])
        # Higher cap allows prime peak; lower cap trims prime proportional to excess above 0.5*max
        effective_prime_shift = (cap_ratio - 1.0) * max(0.0, prime_obs - 1.4) * 0.65 + floor_ratio * max(0.0, 1.8 - prime_obs) * 0.45
        dT_base = float(ds.y_t1[idx] - T_obs)
        dT_cf = dT_base + prime_coef * effective_prime_shift
        T_next_cf = float(np.clip(T_cf + dT_cf, 20.0, 108.0))

        rew, parts = compute_step_reward(
            act=act,
            prev_act=prev_act,
            cpu_next=float(ds.y_cpu_next[idx]),
            prime_next=float(df["prime_mhz"].iloc[idx + 1] if idx + 1 < len(df) else df["prime_mhz"].iloc[idx]),
            T_cf=T_next_cf,
            T_batt=float(df["batt_temp_c"].iloc[idx]),
            psi_full=float(df["psi_full10"].iloc[idx]),
            mem_pct=float(df["mem_avail_pct"].iloc[idx]),
            is_launcher=float(df["is_launcher"].iloc[idx]),
            is_charging=float(df["is_charging"].iloc[idx]),
            batt_pct=float(df["batt_pct"].iloc[idx]),
        )
        if online_update_fn is not None:
            online_update_fn(step_k, idx, T_cf, act, rew)

        rewards.append(rew)
        perfs.append(parts["perf"])
        therms.append(parts["thermal_pen"])
        wastes.append(parts["waste_pen"])
        temps.append(T_next_cf)
        if T_next_cf >= 75.0:
            overheat_mins += 1

        T_cf = T_next_cf
        prev_act = act

    n = max(len(rewards), 1)
    # Learning curve across 4 quartiles of the simulated timeline
    q = max(n // 4, 1)
    quartile_means = [
        round(float(np.mean(rewards[i * q : (i + 1) * q if i < 3 else n])), 4)
        for i in range(4)
    ]
    return {
        "n_steps": int(n),
        "mean_reward": round(float(np.mean(rewards)), 4),
        "total_reward": round(float(np.sum(rewards)), 2),
        "mean_perf_score": round(float(np.mean(perfs)), 4),
        "mean_thermal_pen": round(float(np.mean(therms)), 4),
        "mean_waste_pen": round(float(np.mean(wastes)), 4),
        "overheat_ge75_pct": round(overheat_mins / n * 100.0, 2),
        "mean_temp_c": round(float(np.mean(temps)), 2),
        "p95_temp_c": round(float(np.percentile(temps, 95)), 2),
        "switch_rate_pct": round(switches / n * 100.0, 2),
        "quartile_mean_rewards": quartile_means,
        "action_share": {ACTION_NAMES[i]: round(act_counts[i] / n, 4) for i in range(len(ACTION_NAMES))},
    }


class LinUCBBandit:
    """Disjoint Contextual LinUCB with exponential forgetting factor for non-stationary adaptation."""

    def __init__(
        self,
        d: int,
        n_actions: int = 5,
        alpha: float = 0.6,
        gamma: float = 0.999,
        init_lambda: float = 2.0,
    ) -> None:
        self.d = d
        self.n_actions = n_actions
        self.alpha = alpha
        self.gamma = gamma
        self.A = [np.eye(d, dtype=np.float64) * init_lambda for _ in range(n_actions)]
        self.b = [np.zeros(d, dtype=np.float64) for _ in range(n_actions)]

    def select(
        self,
        x: np.ndarray,
        prior_probs: np.ndarray | None = None,
        prior_w: float = 0.0,
        base_q: np.ndarray | None = None,
    ) -> int:
        best_a = 1
        best_ucb = -1e18
        for a in range(self.n_actions):
            A_inv = np.linalg.inv(self.A[a])
            theta = A_inv @ self.b[a]
            mean = float(theta @ x)
            bonus = self.alpha * float(np.sqrt(max(x @ A_inv @ x, 0.0)))
            prior = float(prior_probs[a]) * prior_w if prior_probs is not None else 0.0
            q_offset = float(base_q[a]) if base_q is not None else 0.0
            score = q_offset + mean + bonus + prior
            if score > best_ucb:
                best_ucb = score
                best_a = a
        return best_a

    def update(self, x: np.ndarray, a: int, r: float) -> None:
        for k in range(self.n_actions):
            self.A[k] *= self.gamma
            self.b[k] *= self.gamma
        self.A[a] += np.outer(x, x)
        self.b[a] += r * x


def build_compact_context(ds) -> np.ndarray:
    df = ds.df_valid
    raw = np.column_stack([
        df["cpu_pct"].values / 100.0,
        df["prime_mhz_norm"].values / 4.5,
        df["cpu_temp_c"].values / 100.0,
        df["d_temp_1m"].values / 10.0,
        df["d_cpu_1m"].values / 50.0,
        df["mem_avail_pct"].values / 100.0,
        df["psi_some10"].values / 30.0,
        df["psi_full10"].values / 10.0,
        df["is_launcher"].values,
        df["is_charging"].values,
        df["batt_pct_norm"].values,
        df["app_cpu_mean"].values / 50.0,
        df["cpu_mean_5"].values / 50.0,
        df["temp_mean_5"].values / 100.0,
        np.ones(len(df), dtype=np.float64),
    ])
    return np.nan_to_num(raw, nan=0.0)


def fit_fqi_policy(ds, ctx_all: np.ndarray, env_reg: Ridge, prime_coef: float) -> list[Ridge]:
    """Fitted Q-Iteration (3 Bellman iterations) over compact state + counterfactual immediate reward."""
    tr = ds.train_idx
    df = ds.df_valid
    n_actions = len(ACTION_NAMES)

    # Precompute 1-step transition tuples inside train segments
    segs = df["seg_id"].values
    valid_tr = [i for i in tr[:-1] if segs[i] == segs[i + 1]]
    X_cur = ctx_all[valid_tr]
    X_nxt = ctx_all[np.array(valid_tr) + 1]

    # Precompute immediate counterfactual rewards for each (state, action) in train
    R_mat = np.zeros((len(valid_tr), n_actions), dtype=np.float64)
    for row_k, idx in enumerate(valid_tr):
        T_obs = float(df["cpu_temp_c"].iloc[idx])
        prime_obs = float(df["prime_mhz_norm"].iloc[idx])
        dT_base = float(ds.y_t1[idx] - T_obs)
        for a in range(n_actions):
            umin, umax = ACTION_UCLAMP[a]
            cap_ratio = umax / 1024.0
            floor_ratio = umin / 1024.0
            shift = (cap_ratio - 1.0) * max(0.0, prime_obs - 1.4) * 0.65 + floor_ratio * max(0.0, 1.8 - prime_obs) * 0.45
            T_next_cf = float(np.clip(T_obs + dT_base + prime_coef * shift, 20.0, 108.0))
            r, _ = compute_step_reward(
                act=a,
                prev_act=int(ds.y_action[idx - 1]) if idx > 0 else 1,
                cpu_next=float(ds.y_cpu_next[idx]),
                prime_next=float(df["prime_mhz"].iloc[idx + 1]),
                T_cf=T_next_cf,
                T_batt=float(df["batt_temp_c"].iloc[idx]),
                psi_full=float(df["psi_full10"].iloc[idx]),
                mem_pct=float(df["mem_avail_pct"].iloc[idx]),
                is_launcher=float(df["is_launcher"].iloc[idx]),
                is_charging=float(df["is_charging"].iloc[idx]),
                batt_pct=float(df["batt_pct"].iloc[idx]),
            )
            R_mat[row_k, a] = r

    # Bellman iterations: Q_k(s, a) = r(s, a) + gamma * max_{a'} Q_{k-1}(s', a')
    gamma_rl = 0.85
    q_models = [Ridge(alpha=5.0, fit_intercept=False) for _ in range(n_actions)]
    for a in range(n_actions):
        q_models[a].fit(X_cur, R_mat[:, a])

    for _ in range(4):
        next_q = np.column_stack([q_models[a].predict(X_nxt) for a in range(n_actions)])
        max_next_q = next_q.max(axis=1)
        new_models = []
        for a in range(n_actions):
            target = R_mat[:, a] + gamma_rl * max_next_q
            new_models.append(Ridge(alpha=5.0, fit_intercept=False).fit(X_cur, target))
        q_models = new_models
    return q_models


def run_tiny_mlp_probs(ds, weights_path: Path) -> np.ndarray:
    with open(weights_path, encoding="utf-8") as f:
        w = json.load(f)
    mean = np.array(w["scaler_mean"], dtype=np.float64)
    std = np.array(w["scaler_std"], dtype=np.float64)
    X = (ds.X_tab.astype(np.float64) - mean) / np.maximum(std, 1e-4)
    L = w["layers"]
    fc1 = np.maximum(0.0, X @ np.array(L["fc1_w"]).T + np.array(L["fc1_b"]))
    fc2 = np.maximum(0.0, fc1 @ np.array(L["fc2_w"]).T + np.array(L["fc2_b"]))
    logits = fc2 @ np.array(L["act_w"]).T + np.array(L["act_b"])
    logits -= logits.max(axis=1, keepdims=True)
    probs = np.exp(logits)
    return probs / probs.sum(axis=1, keepdims=True)


def run_compact_gbdt_probs(ds, model_path: Path) -> tuple[np.ndarray, np.ndarray]:
    with open(model_path, encoding="utf-8") as f:
        m = json.load(f)
    action_trees = m["heads"]["action"]
    X = ds.X_tab.astype(np.float64)
    logits = np.zeros((len(X), len(ACTION_NAMES)), dtype=np.float64)
    for row_i in range(len(X)):
        row = X[row_i]
        for c in range(len(ACTION_NAMES)):
            s = 0.0
            for tr in action_trees[c]:
                idx = 0
                f_arr, t_arr, l_arr, r_arr, v_arr = tr["f"], tr["t"], tr["l"], tr["r"], tr["v"]
                while f_arr[idx] >= 0:
                    idx = l_arr[idx] if row[f_arr[idx]] <= t_arr[idx] else r_arr[idx]
                s += v_arr[idx]
            logits[row_i, c] = s
    logits -= logits.max(axis=1, keepdims=True)
    probs = np.exp(logits)
    probs /= probs.sum(axis=1, keepdims=True)
    fqi_w = np.array(m["fqi"]["weights"], dtype=np.float64)
    return probs, fqi_w


def main() -> None:
    ds = build_dataset()
    env_reg, prime_coef, _ = fit_thermal_env(ds)
    ctx_all = build_compact_context(ds)

    with open(ART_DIR / "lgb_models.pkl", "rb") as f:
        lgb_bundle = pickle.load(f)
    lgb_act_probs = lgb_bundle["action"].predict_proba(ds.X_tab)
    tiny_act_probs = run_tiny_mlp_probs(ds, ART_DIR / "tiny_mlp_weights.json")

    sched_path = ART_DIR / "sched_model.json"
    compact_probs = None
    fqi_weights = None
    if sched_path.exists():
        compact_probs, fqi_weights = run_compact_gbdt_probs(ds, sched_path)

    fqi_models = fit_fqi_policy(ds, ctx_all, env_reg, prime_coef)
    if fqi_weights is None:
        fqi_weights = np.array([m.coef_ for m in fqi_models], dtype=np.float64)

    te = ds.test_idx
    full_idx = np.arange(len(ds.df_valid), dtype=np.int64)
    df = ds.df_valid

    def static_rule_fn(_k, idx, T_cf, _prev):
        if T_cf >= 75.0 or df["psi_full10"].iloc[idx] >= 3.0 or df["mem_avail_pct"].iloc[idx] < 8.0:
            return 4
        if (df["is_charging"].iloc[idx] < 0.5 and df["batt_pct"].iloc[idx] <= 15.0) or (
            df["is_launcher"].iloc[idx] > 0.5 and df["cpu_pct"].iloc[idx] < 4.5 and df["prime_mhz"].iloc[idx] <= 1200.0
        ):
            return 0
        if df["cpu_pct"].iloc[idx] >= 25.0 or df["prime_mhz"].iloc[idx] >= 2400.0:
            return 3
        if df["is_launcher"].iloc[idx] < 0.5 and df["cpu_pct"].iloc[idx] >= 5.0:
            return 2
        return 1

    def old_static_rule_fn(_k, idx, T_cf, _prev):
        # Old pre-calibration 50C moderate threshold
        if T_cf >= 50.0 or df["psi_full10"].iloc[idx] >= 3.0 or df["mem_avail_pct"].iloc[idx] < 8.0:
            return 4
        return static_rule_fn(_k, idx, T_cf, _prev)

    def lgb_fn(_k, idx, T_cf, _prev):
        if T_cf >= 75.0:
            return 4
        return int(np.argmax(lgb_act_probs[idx]))

    def tiny_mlp_fn(_k, idx, T_cf, _prev):
        if T_cf >= 75.0:
            return 4
        return int(np.argmax(tiny_act_probs[idx]))

    def compact_gbdt_fn(_k, idx, T_cf, _prev):
        if T_cf >= 75.0:
            return 4
        return int(np.argmax(compact_probs[idx]))

    def fqi_fn(_k, idx, T_cf, _prev):
        x = ctx_all[idx].copy()
        x[2] = T_cf / 100.0
        if T_cf >= 78.0:
            return 4
        qs = [float(m.predict(x[None, :])[0]) for m in fqi_models]
        return int(np.argmax(qs))

    def make_fqi_residual_bandit():
        bandit = LinUCBBandit(d=ctx_all.shape[1], alpha=0.15, gamma=0.999, init_lambda=8.0)

        def select_fn(_k, idx, T_cf, _prev):
            x = ctx_all[idx].copy()
            x[2] = T_cf / 100.0
            if T_cf >= 78.0 or df["psi_full10"].iloc[idx] >= 3.0 or df["mem_avail_pct"].iloc[idx] < 8.0:
                return 4
            base_q = fqi_weights @ x
            adv_prior = (base_q - float(np.mean(base_q))) * 1.5
            return bandit.select(x, base_q=adv_prior)

        def update_fn(_k, idx, T_cf, act, rew):
            x = ctx_all[idx].copy()
            x[2] = T_cf / 100.0
            bandit.update(x, act, rew)

        return select_fn, update_fn

    res_test_sel, res_test_upd = make_fqi_residual_bandit()

    # 1. Blind Test Set Simulation (same test set as all supervised models)
    test_res = {
        "OldStatic_50C": simulate_policy(ds, te, old_static_rule_fn, env_reg, prime_coef),
        "StaticRules_v012": simulate_policy(ds, te, static_rule_fn, env_reg, prime_coef),
        "LightGBM_Policy": simulate_policy(ds, te, lgb_fn, env_reg, prime_coef),
        "TinyMLP_Policy": simulate_policy(ds, te, tiny_mlp_fn, env_reg, prime_coef),
    }
    if compact_probs is not None:
        test_res["CompactGBDT_Policy"] = simulate_policy(ds, te, compact_gbdt_fn, env_reg, prime_coef)
    test_res["OfflineRL_FQI"] = simulate_policy(ds, te, fqi_fn, env_reg, prime_coef)
    test_res["WarmStart_FQI_Residual_LinUCB"] = simulate_policy(
        ds, te, res_test_sel, env_reg, prime_coef, online_update_fn=res_test_upd
    )

    # 2. Full 17-Day Online Continuous Learning Simulation (Bandit & WarmStart Bandits)
    bandit_cold = LinUCBBandit(d=ctx_all.shape[1], alpha=0.55, gamma=0.999)

    def cold_select(_k, idx, T_cf, _prev):
        x = ctx_all[idx].copy()
        x[2] = T_cf / 100.0
        if T_cf >= 80.0:
            return 4
        return bandit_cold.select(x)

    def cold_update(_k, idx, T_cf, act, rew):
        x = ctx_all[idx].copy()
        x[2] = T_cf / 100.0
        bandit_cold.update(x, act, rew)

    bandit_warm = LinUCBBandit(d=ctx_all.shape[1], alpha=0.35, gamma=0.999)

    def warm_select(_k, idx, T_cf, _prev):
        x = ctx_all[idx].copy()
        x[2] = T_cf / 100.0
        if T_cf >= 80.0:
            return 4
        return bandit_warm.select(x, prior_probs=tiny_act_probs[idx], prior_w=1.2)

    def warm_update(_k, idx, T_cf, act, rew):
        x = ctx_all[idx].copy()
        x[2] = T_cf / 100.0
        bandit_warm.update(x, act, rew)

    res_full_sel, res_full_upd = make_fqi_residual_bandit()

    full_res = {
        "StaticRules_v012": simulate_policy(ds, full_idx, static_rule_fn, env_reg, prime_coef),
        "LinUCB_ColdStart": simulate_policy(ds, full_idx, cold_select, env_reg, prime_coef, online_update_fn=cold_update),
        "WarmStart_TinyMLP_LinUCB": simulate_policy(ds, full_idx, warm_select, env_reg, prime_coef, online_update_fn=warm_update),
        "WarmStart_FQI_Residual_LinUCB": simulate_policy(ds, full_idx, res_full_sel, env_reg, prime_coef, online_update_fn=res_full_upd),
        "OfflineRL_FQI": simulate_policy(ds, full_idx, fqi_fn, env_reg, prime_coef),
    }

    # Export LinUCB + FQI lightweight params as well for potential Rust inspection
    out = {
        "blind_test_simulation": test_res,
        "full_17d_continuous_learning": full_res,
    }
    with open(RES_DIR / "rl_sim_results.json", "w", encoding="utf-8") as f:
        json.dump(out, f, indent=2, ensure_ascii=False)

    # Also save FQI linear Q weights into artifact
    with open(ART_DIR / "fqi_weights.json", "w", encoding="utf-8") as f:
        json.dump(
            {
                "action_names": ACTION_NAMES,
                "weights": [np.round(m.coef_, 6).tolist() for m in fqi_models],
            },
            f,
            indent=2,
        )

    print("=== Blind Test Simulation ===")
    for k, v in test_res.items():
        print(f"  {k:20s}: mean_rew={v['mean_reward']:+.4f} perf={v['mean_perf_score']:+.4f} therm={v['mean_thermal_pen']:.4f} waste={v['mean_waste_pen']:.4f} overheat={v['overheat_ge75_pct']:.1f}% switch={v['switch_rate_pct']:.1f}%")
    print("=== 17-Day Continuous Learning Curve (4 Quartiles) ===")
    for k, v in full_res.items():
        print(f"  {k:24s}: mean_rew={v['mean_reward']:+.4f} quartiles={v['quartile_mean_rewards']} overheat={v['overheat_ge75_pct']:.1f}%")


if __name__ == "__main__":
    main()

/* NeuroSched WebUI — no framework, no CDN, no shell. */
(() => {
  "use strict";

  let token = "";
  const $ = (id) => document.getElementById(id);
  const el = (tag, cls, text) => {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined) n.textContent = text;
    return n;
  };
  const fmt = (v, d = 0) => (v === null || v === undefined || Number.isNaN(v) ? "—" : Number(v).toFixed(d));
  const pct = (v, d = 0) => (v === null || v === undefined ? "—" : `${Number(v).toFixed(d)}%`);

  const state = { status: null, history: [], tab: "dashboard", busy: false, polling: false };

  async function request(path, opts) {
    if (!token) throw new Error("请输入 API 凭证后连接");
    const options = Object.assign({ cache: "no-store" }, opts || {});
    options.headers = new Headers(options.headers || {});
    options.headers.set("X-NeuroSched-Token", token);
    const res = await fetch(`/api/${path}`, options);
    if (res.status === 401) {
      token = "";
      $("auth-message").textContent = "鉴权失败，请重新输入 API 凭证。";
      throw new Error("API 凭证无效或已更换");
    }
    if (!res.ok) {
      const detail = await res.json().catch(() => null);
      throw new Error(detail && detail.error ? detail.error : `${path}: HTTP ${res.status}`);
    }
    return res;
  }

  async function api(path, opts) {
    const res = await request(path, opts);
    const text = await res.text();
    return text ? JSON.parse(text) : null;
  }

  /* ---------------- tabs ---------------- */
  function initTabs() {
    document.querySelectorAll("#tabs button").forEach((b) => {
      b.addEventListener("click", () => {
        document.querySelectorAll("#tabs button").forEach((x) => x.classList.remove("active"));
        b.classList.add("active");
        state.tab = b.dataset.tab;
        document.querySelectorAll(".tab").forEach((s) => s.classList.remove("active"));
        const target = $(`tab-${state.tab}`);
        if (target) target.classList.add("active");
        refreshTab();
      });
    });
  }

  function refreshTab() {
    if (state.tab === "logs") loadLogs();
    if (state.tab === "safety") loadSafety();
    if (state.tab === "policy") loadPolicy();
    if (state.tab === "predict") loadPredict();
  }

  /* ---------------- poll ---------------- */
  async function poll() {
    if (!token || state.polling || document.hidden) return;
    state.polling = true;
    try {
      const s = await api("status");
      state.status = s;
      $("conn-dot").className = "dot on";
      renderHeader(s);
      if (state.tab === "dashboard") renderDashboard(s);
      if (state.tab === "cpu") renderCpu(s);
      if (state.tab === "memory") renderMemory(s);
      if (state.tab === "predict") {
        renderPredict(s.predict, s.predictor, s.prediction_policy);
        if (s.learner) renderLearner(s.learner);
      }
      if (state.tab === "policy") renderPolicy(s.policy);
      if (state.tab === "safety") renderSafety(s.safety);
      $("foot-ts").textContent = `更新 ${new Date().toLocaleTimeString()}`;
      $("auth-message").textContent = "已连接。凭证仅在本页内存保存，刷新页面后需重新输入。";
      const hist = await api("history?limit=240").catch(() => null);
      if (Array.isArray(hist)) {
        state.history = hist;
        if (state.tab === "dashboard") drawHistory();
      }
    } catch (e) {
      $("conn-dot").className = "dot off";
      $("foot-ts").textContent = `连接失败：${e.message}`;
    } finally {
      state.polling = false;
    }
  }

  /* ---------------- header ---------------- */
  function renderHeader(s) {
    $("ver").textContent = `v${s.version}`;
    const d = s.device || {};
    $("chip-device").textContent = d.model ? `${d.model} · ${d.soc_model || d.board || ""}` : "device n/a";
    const configured = s.configured_mode || s.mode;
    $("chip-mode").textContent = configured === s.mode ? s.mode : `${s.mode} (cfg ${configured})`;
    $("chip-mode").title = configured === s.mode
      ? "运行模式"
      : "已因保护条件降级；配置文件中的模式保持不变";
    $("chip-wd").textContent = `wd: ${(s.safety || {}).watchdog || "?"}`;
    // Thermal fail-closed banner: visible on every tab.
    const fc = s.thermal && s.thermal.fail_closed;
    const banner = $("failclosed");
    if (banner) {
      banner.hidden = !fc;
      if (fc) {
        $("failclosed-reason").textContent =
          `${(s.thermal.trust || "unknown")} · ${s.thermal.fail_closed_reason || s.thermal.trust_detail || ""}`;
      }
    }

    const st = $("chip-state");
    st.textContent = s.policy.state;
    st.className = "chip state";
    if (s.policy.state === "THERMAL_GUARD" || s.policy.state === "MEMORY_PRESSURE") st.classList.add("warn");
    if (s.policy.state === "SAFE_MODE") st.classList.add("bad");
  }

  /* ---------------- dashboard ---------------- */
  function renderDashboard(s) {
    const own = s.daemon || {};
    const ownPanel = $("d-daemon");
    ownPanel.textContent = "";
    kvRows(ownPanel, [
      ["Daemon CPU", pct(own.cpu_pct, 2)],
      ["常驻内存", own.rss_kb == null ? "—" : `${fmt(own.rss_kb / 1024, 2)} MiB`],
      ["线程数", b0(own.threads, "")],
      ["主线程上下文切换/s", fmt(own.context_switches_per_sec, 1)],
      ["已完成采集轮次", b0(own.completed_ticks, "")],
      ["本轮 / 最慢耗时", `${fmt(own.last_tick_ms, 2)} / ${fmt(own.max_tick_ms, 2)} ms`],
      ["资源数据年龄", own.observed_mono_ms == null ? "—" : `${fmt(Math.max(0, s.ts_ms - own.observed_mono_ms) / 1000, 1)} s`],
    ]);
    $("d-state").textContent = s.policy.state;
    $("d-reason").textContent = s.policy.reason;
    $("d-cpu").textContent = fmt(s.cpu.total_pct, 0);
    $("d-cpu-sub").textContent = (s.cpu.clusters || [])
      .map((c) => `${c.name} ${fmt(c.cur_mhz, 0)}MHz`)
      .join(" · ");
    $("d-mem").textContent = fmt(s.memory.available_mb, 0);
    $("d-mem-sub").textContent = `swap ${fmt(s.memory.swap_used_mb, 0)}MB · PSI full ${fmt(s.psi.memory.full10, 2)}`;
    $("d-temp").textContent = s.thermal.cpu_c === null ? "—" : fmt(s.thermal.cpu_c, 1);
    $("d-temp-sub").textContent = s.thermal.trust === "trusted"
      ? `skin ${fmt(s.thermal.skin_c, 1)} · batt ${fmt(s.battery.temp_c, 1)} · ${s.thermal.status}`
      : `温度不可信：${s.thermal.trust}（已禁止 boost）`;

    const wrap = $("d-clusters");
    wrap.textContent = "";
    (s.cpu.clusters || []).forEach((c) => {
      const box = el("div", "cluster");
      const top = el("div", "cluster-top");
      top.appendChild(el("b", null, `${c.name} · ${c.cpus}`));
      top.appendChild(el("span", "freq", `${fmt(c.cur_mhz, 0)} / ${fmt(c.max_mhz, 0)} MHz`));
      const track = el("div", "track" + (c.load_pct > 80 ? " hot" : ""));
      const fill = el("i");
      fill.style.width = `${Math.min(100, c.load_pct)}%`;
      track.appendChild(fill);
      box.appendChild(top);
      box.appendChild(track);
      wrap.appendChild(box);
    });
    $("d-cluster-hint").textContent = `${(s.cpu.clusters || []).length} 个频率策略`;

    const p = $("d-pressure");
    p.textContent = "";
    const rows = [
      ["PSI mem some10", s.psi.memory.some10, 40, "hint"],
      ["PSI mem full10", s.psi.memory.full10, 20, "hint"],
      ["kswapd0 %", s.memory.kswapd_pct, 100, "hint"],
      ["swap in KiB/s", s.memory.swap_in_rate, 8192, "hint"],
    ];
    rows.forEach(([name, val, max]) => p.appendChild(barRow(name, val, max, fmt(val, 2))));

    const b = $("d-battery");
    b.textContent = "";
    kvRows(b, [
      ["电量", b0(s.battery.pct, "%")],
      ["状态", s.battery.status || "—"],
      ["温度", `${fmt(s.battery.temp_c, 1)} °C`],
      ["电流", s.battery.current_ma === null ? "—" : `${fmt(s.battery.current_ma, 0)} mA`],
      ["电压", s.battery.voltage_mv === null ? "—" : `${fmt(s.battery.voltage_mv, 0)} mV`],
    ]);

    const h = $("d-health");
    h.textContent = "";
    (s.health || []).forEach((c) => {
      const k = el("div", null, c.name);
      const v = el("div", c.ok ? "ok" : "bad", `${c.ok ? "ok" : "fail"} · ${c.note}`);
      h.appendChild(k);
      h.appendChild(v);
    });
  }

  const b0 = (v, unit) => (v === null || v === undefined ? "—" : `${Number(v).toFixed(0)}${unit}`);

  function barRow(name, val, max, text) {
    const row = el("div", "bar-row");
    row.appendChild(el("span", "name", name));
    const track = el("div", "track");
    const fill = el("i");
    fill.style.width = `${Math.max(0, Math.min(100, (Number(val) / max) * 100))}%`;
    if (val / max > 0.75) fill.style.background = "linear-gradient(90deg,#ffb347,#ff6b6b)";
    track.appendChild(fill);
    row.appendChild(track);
    row.appendChild(el("span", "val", text));
    return row;
  }

  function kvRows(parent, rows) {
    rows.forEach(([k, v]) => {
      parent.appendChild(el("div", null, k));
      parent.appendChild(el("div", null, String(v)));
    });
  }

  /* ---------------- history chart ---------------- */
  function drawHistory() {
    const cv = $("chart-history");
    if (!cv) return;
    const dpr = window.devicePixelRatio || 1;
    const w = cv.clientWidth || 600;
    const h = 180;
    cv.width = Math.floor(w * dpr);
    cv.height = Math.floor(h * dpr);
    const ctx = cv.getContext("2d");
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);

    const data = state.history;
    if (!data.length) {
      ctx.fillStyle = "#6b7f95";
      ctx.font = "12px system-ui";
      ctx.fillText("暂无历史数据（每分钟聚合一次）", 12, h / 2);
      return;
    }

    ctx.strokeStyle = "rgba(255,255,255,.05)";
    ctx.lineWidth = 1;
    for (let i = 0; i <= 4; i++) {
      const y = (h / 4) * i + 0.5;
      ctx.beginPath();
      ctx.moveTo(0, y);
      ctx.lineTo(w, y);
      ctx.stroke();
    }

    const series = [
      { key: "cpu", color: "#4cc2ff", max: 100 },
      { key: "psi_some10", color: "#ffc857", max: 40 },
      { key: "cpu_temp", color: "#ff6b6b", max: 90 },
      { key: "mem_avail_mb", color: "#3ddc97", max: Math.max(1, ...data.map((d) => d.mem_avail_mb || 0)) },
    ];
    const n = data.length;
    series.forEach((s) => {
      ctx.beginPath();
      ctx.strokeStyle = s.color;
      ctx.lineWidth = 1.6;
      data.forEach((d, i) => {
        const x = n === 1 ? 0 : (i / (n - 1)) * (w - 2) + 1;
        const v = Math.max(0, Math.min(s.max, d[s.key] || 0)) / s.max;
        const y = h - 6 - v * (h - 12);
        if (i === 0) ctx.moveTo(x, y);
        else ctx.lineTo(x, y);
      });
      ctx.stroke();
    });
    $("d-hist-hint").textContent = `${n} 点 · 最新 ${new Date(data[data.length - 1].ts).toLocaleTimeString()}`;
  }

  /* ---------------- cpu ---------------- */
  function renderCpu(s) {
    const t = $("cpu-clusters");
    t.textContent = "";
    t.appendChild(thead(["集群", "CPU", "Governor", "当前", "最小", "最大", "峰值", "容量", "负载"]));
    const tb = el("tbody");
    (s.cpu.clusters || []).forEach((c) => {
      const tr = el("tr");
      [c.name, c.cpus, c.governor, `${fmt(c.cur_mhz, 0)}MHz`, `${fmt(c.min_mhz, 0)}MHz`, `${fmt(c.max_mhz, 0)}MHz`, `${fmt(c.cpuinfo_max_mhz, 0)}MHz`, c.capacity || "—", pct(c.load_pct)]
        .forEach((v, i) => tr.appendChild(el("td", i >= 3 && i <= 7 ? "mono" : null, String(v))));
      tb.appendChild(tr);
    });
    t.appendChild(tb);
    $("cpu-topo-hint").textContent = `present ${s.cpu.present} · online ${s.cpu.online}`;

    const cores = $("cpu-cores");
    cores.textContent = "";
    (s.cpu.cores || []).forEach((c) => {
      const box = el("div", "core");
      const id = el("div", "id");
      id.appendChild(el("span", null, `cpu${c.id}`));
      id.appendChild(el("span", null, `${fmt(c.mhz, 0)}MHz`));
      box.appendChild(id);
      box.appendChild(el("div", "pct", pct(c.pct)));
      const track = el("div", "track" + (c.pct > 85 ? " hot" : ""));
      const fill = el("i");
      fill.style.width = `${Math.min(100, c.pct)}%`;
      track.appendChild(fill);
      box.appendChild(track);
      cores.appendChild(box);
    });

    const sch = $("cpu-sched");
    sch.textContent = "";
    kvRows(sch, [
      ["WALT", s.cpu.walt ? "存在" : "不存在"],
      ["uclamp", s.cpu.uclamp ? "可探测" : "不可用"],
      ["集群数", (s.cpu.clusters || []).length],
      ["采样", `${(state.status.config || {}).mode || s.mode}`],
    ]);

    const knobs = $("cpu-knobs");
    knobs.textContent = "";
    const entries = Object.entries(s.cpu.knobs || {});
    if (!entries.length) knobs.appendChild(el("div", "empty", "未发现可读的调度节点"));
    entries.forEach(([k, v]) => {
      knobs.appendChild(el("div", null, k));
      knobs.appendChild(el("div", null, v));
    });
  }

  /* ---------------- memory ---------------- */
  function renderMemory(s) {
    const m = s.memory;
    $("m-avail").textContent = fmt(m.available_mb, 0);
    $("m-avail-pct").textContent = `${fmt(m.available_pct, 1)}% / ${fmt(m.total_mb, 0)}MB`;
    $("m-swap").textContent = fmt(m.swap_used_mb, 0);
    $("m-swap-sub").textContent = `总 ${fmt(m.swap_total_mb, 0)}MB · ${m.swap_devices || "无 swap"}`;
    const z = (m.zram || [])[0];
    $("m-ratio").textContent = z ? fmt(z.ratio, 2) : "—";
    $("m-ratio-sub").textContent = z ? `${fmt(z.orig_mb, 0)}MB → ${fmt(z.compr_mb, 0)}MB` : "无 zRAM";
    $("m-kswapd").textContent = fmt(m.kswapd_pct, 1);
    $("m-kswapd-sub").textContent = `pid ${m.kswapd_pid || "—"} · scan ${fmt(m.pgscan_kswapd, 0)}`;

    const det = $("m-detail");
    det.textContent = "";
    kvRows(det, [
      ["MemTotal", `${fmt(m.total_mb, 0)} MB`],
      ["MemAvailable", `${fmt(m.available_mb, 0)} MB`],
      ["Cached", `${fmt(m.cached_mb, 0)} MB`],
      ["Swap used", `${fmt(m.swap_used_mb, 0)} MB`],
      ["pswpin", fmt(m.pswpin, 0)],
      ["pswpout", fmt(m.pswpout, 0)],
      ["swap in rate", `${fmt(m.swap_in_rate, 0)} KiB/s`],
      ["pgmajfault", fmt(m.pgmajfault, 0)],
      ["分类", s.policy.memory_class],
    ]);

    const psi = $("m-psi");
    psi.textContent = "";
    [["mem some", s.psi.memory.some10, 40], ["mem full", s.psi.memory.full10, 20], ["cpu some", s.psi.cpu.some10, 100], ["io some", s.psi.io.some10, 100]]
      .forEach(([n, v, max]) => psi.appendChild(barRow(n, v, max, fmt(v, 2))));

    const zt = $("m-zram");
    zt.textContent = "";
    zt.appendChild(thead(["设备", "原始", "压缩后", "实际占用", "压缩比"]));
    const zb = el("tbody");
    (m.zram || []).forEach((d) => {
      const tr = el("tr");
      [d.name, `${fmt(d.orig_mb, 0)}MB`, `${fmt(d.compr_mb, 0)}MB`, `${fmt(d.used_mb, 0)}MB`, `${fmt(d.ratio, 2)}×`]
        .forEach((v) => tr.appendChild(el("td", "mono", v)));
      zb.appendChild(tr);
    });
    zt.appendChild(zb);

    const mmd = $("m-mmd");
    mmd.textContent = "";
    kvRows(mmd, [
      ["存在", m.mmd.present ? "是" : "否"],
      ["PID", m.mmd.pid || "—"],
      ["init.svc.mmd", m.mmd.service || "—"],
      ["策略", "只监控，不接管"],
    ]);
  }

  /* ---------------- predictor ---------------- */
  function renderPredict(p, stats, preview = p.policy_preview) {
    $("p-pkg").textContent = p.predicted_package || "（拒绝预测）";
    $("p-prob").textContent = `${(p.probability * 100).toFixed(1)}%`;
    $("p-conf").textContent = `${(p.confidence * 100).toFixed(0)}%`;
    $("p-samples").textContent = p.sample_count;
    $("p-acc").textContent = stats && stats.evaluated > 0 ? `${(stats.accuracy * 100).toFixed(0)}%` : "待结算";
    $("p-reason").textContent = p.reason || "";
    $("p-feedback").textContent = stats
      ? `机会 ${stats.opportunities || 0} · 覆盖 ${pct((stats.coverage || 0) * 100, 0)} · 已发出 ${stats.predictions} · 已结算 ${stats.evaluated} · Top1 ${stats.hits} / Top3 ${stats.top3_hits || 0} (${pct((stats.top3_accuracy || 0) * 100, 0)}) · 有效命中 ${pct((stats.effective_accuracy || 0) * 100, 0)} · 超时 ${stats.expired}`
      : "尚无评估统计";
    if ($("p-baseline")) {
      const reasons = stats && stats.refused_reasons
        ? Object.entries(stats.refused_reasons).map(([k, v]) => `${k}:${v}`).join(" ")
        : "";
      $("p-baseline").textContent = stats && stats.evaluated > 0
        ? `基线对比 · 高频 ${pct((stats.baseline_most_frequent_accuracy || 0) * 100, 1)} (${stats.baseline_most_frequent_hits || 0}) · 回切 ${pct((stats.baseline_previous_accuracy || 0) * 100, 1)} (${stats.baseline_previous_hits || 0})${reasons ? ` · 拒绝 ${reasons}` : ""}`
        : `基线对比待结算${reasons ? ` · 拒绝 ${reasons}` : ""}`;
    }
    $("p-preview").textContent = preview && preview.eligible && preview.profile
      ? `未来目标只读模拟 ${preview.target_package} · uclamp ${preview.profile.uclamp_min}/${preview.profile.uclamp_max}（0–1024）· ${preview.duration_ms} ms`
      : `未来目标模拟被阻断 · ${preview ? (preview.blocked || []).join(" / ") || preview.reason : "等待采样"}`;
    $("p-refused").textContent = p.refused ? "已拒绝（样本或置信度不足）" : "已给出预测";

    const c = $("p-candidates");
    c.textContent = "";
    (p.candidates || []).forEach((x) => {
      const row = el("div", "cand-row");
      row.appendChild(el("span", "pkg", x.package));
      const track = el("div", "track");
      const fill = el("i");
      fill.style.width = `${Math.min(100, x.score * 100)}%`;
      track.appendChild(fill);
      row.appendChild(track);
      row.appendChild(el("span", "val", `${(x.score * 100).toFixed(1)}%`));
      c.appendChild(row);
    });
  }

  function renderLearner(l) {
    if (!l || !$("l-source")) return;
    $("l-source").textContent = `${l.model_source || "embedded"} · ${l.architecture || "CompactGBDT_FQI"}`;
    $("l-cpu").textContent = `${fmt(l.pred_cpu_pct, 1)}%`;
    $("l-burst").textContent = pct((l.burst_prob || 0) * 100, 1);
    $("l-temp").textContent = `${fmt(l.pred_t1_c, 1)} / ${fmt(l.pred_t3_c, 1)} °C`;
    $("l-oh").textContent = pct((l.overheat_3m_prob || 0) * 100, 1);

    const act = $("l-action");
    act.textContent = "";
    kvRows(act, [
      ["AI 推荐动作", `${l.recommended_action || "—"} (uclamp ${b0(l.uclamp_min, "")}/${b0(l.uclamp_max, "")})`],
      ["静态规则对比", l.static_action || "—"],
      ["决策来源", l.decision_source || "—"],
      ["触发阈值", `burst ≥ ${pct((l.th_burst || 0) * 100, 0)} · oh ≥ ${pct((l.th_overheat || 0) * 100, 0)}`],
    ]);

    const sh = l.shadow || {};
    const shadow = $("l-shadow");
    shadow.textContent = "";
    kvRows(shadow, [
      ["Shadow 胜率", `${pct((sh.shadow_win_rate || 0) * 100, 1)} (${sh.shadow_wins || 0}胜 / ${sh.shadow_ties || 0}平 / ${sh.shadow_losses || 0}负)`],
      ["Reward Lift", `${Number(sh.reward_lift || 0) >= 0 ? "+" : ""}${fmt(sh.reward_lift, 4)} (AI ${fmt(sh.learned_mean_reward, 3)} vs 规则 ${fmt(sh.static_mean_reward, 3)})`],
      ["在线更新步数", `${sh.updates || 0} 分钟`],
      ["异常发现 (分钟)", `无效 Boost ${sh.wasted_boost_mins || 0} · 高温≥75°C ${sh.thermal_spike_mins || 0} · 内存高压 ${sh.memory_pressure_mins || 0}`],
    ]);
  }

  async function loadPredict() {
    const [j, l] = await Promise.all([
      api("predict").catch(() => null),
      api("learner").catch(() => null),
    ]);
    if (l) renderLearner(l);
    if (!j) return;
    renderPredict(j, j.stats);
    const t = $("p-usage");
    t.textContent = "";
    t.appendChild(thead(["应用", "次数"]));
    const tb = el("tbody");
    (j.usage || []).forEach((u) => {
      const tr = el("tr");
      tr.appendChild(el("td", "mono", u.package));
      tr.appendChild(el("td", null, String(u.count)));
      tb.appendChild(tr);
    });
    t.appendChild(tb);
  }

  /* ---------------- policy ---------------- */
  const PRIORITY = [
    ["SAFE_MODE", 100], ["THERMAL_GUARD", 90], ["MEMORY_PRESSURE", 80], ["BATTERY_SAVER", 70],
    ["APP_LAUNCH", 60], ["GAME", 50], ["FOREGROUND", 40], ["BALANCED", 30], ["IDLE", 20], ["BOOT", 10],
  ];

  function renderPolicy(p) {
    const cur = $("pol-current");
    cur.textContent = "";
    kvRows(cur, [
      ["状态", p.state],
      ["原因", p.reason],
      ["已保持", `${Math.round(p.dwell_ms / 1000)}s`],
      ["热权限", p.permission],
      ["内存分类", p.memory_class],
      ["uclamp", `min ${p.profile.uclamp_min} / max ${p.profile.uclamp_max}`],
    ]);
    $("pol-blocked").textContent = (p.blocked || []).length ? `受限：${p.blocked.join("；")}` : "无阻塞";
    $("pol-dwell").textContent = `已保持 ${Math.round(p.dwell_ms / 1000)}s`;

    const pr = $("pol-prio");
    pr.textContent = "";
    PRIORITY.forEach(([name, score]) => {
      const row = el("div", "prio-row" + (p.state === name ? " active" : ""));
      row.appendChild(el("span", "idx", String(score)));
      row.appendChild(el("span", "nm", name));
      row.appendChild(el("span", "pr", p.state === name ? "◀" : ""));
      pr.appendChild(row);
    });

    const et = $("pol-eligible");
    et.textContent = "";
    et.appendChild(thead(["状态", "优先级", "触发原因"]));
    const eb = el("tbody");
    (p.eligible || []).slice().sort((a, b) => b.priority - a.priority).forEach((e) => {
      const tr = el("tr");
      tr.appendChild(el("td", "mono", e.state));
      tr.appendChild(el("td", null, String(e.priority)));
      tr.appendChild(el("td", null, e.reason));
      eb.appendChild(tr);
    });
    et.appendChild(eb);

    const tt = $("pol-trans");
    tt.textContent = "";
    tt.appendChild(thead(["单调时钟 ms", "从", "到", "原因"]));
    const tb = el("tbody");
    (p.transitions || []).forEach((t) => {
      const tr = el("tr");
      tr.appendChild(el("td", "mono", String(t.ts)));
      tr.appendChild(el("td", "mono", t.from));
      tr.appendChild(el("td", "mono", t.to));
      tr.appendChild(el("td", null, t.reason));
      tb.appendChild(tr);
    });
    tt.appendChild(tb);
    if (!(p.transitions || []).length) tt.appendChild(el("div", "empty", "尚无状态迁移"));
  }

  const FIELDS = [
    ["policy.thermal.enter_light_c", "热 LIGHT 阈值 °C", "number", 0.5],
    ["policy.thermal.enter_moderate_c", "热 MODERATE 阈值 °C", "number", 0.5],
    ["policy.thermal.enter_severe_c", "热 SEVERE 阈值 °C", "number", 0.5],
    ["policy.thermal.enter_critical_c", "热 CRITICAL 阈值 °C", "number", 0.5],
    ["policy.thermal.exit_margin_c", "退出回差 °C", "number", 0.5],
    ["policy.memory.psi_some_enter", "PSI some 进入", "number", 0.5],
    ["policy.memory.psi_some_exit", "PSI some 退出", "number", 0.5],
    ["policy.memory.psi_full_enter", "PSI full 进入", "number", 0.5],
    ["policy.memory.avail_pct_enter", "可用内存进入 %", "number", 0.5],
    ["policy.battery.saver_pct", "省电阈值 %", "number", 1],
    ["policy.app_launch.boost_ms", "启动窗口 ms", "number", 100],
    ["predictor.min_confidence", "预测置信度下限", "number", 0.05],
    ["predictor.min_samples", "预测最小样本", "number", 1],
  ];

  function getPath(obj, path) {
    return path.split(".").reduce((o, k) => (o && o[k] !== undefined ? o[k] : undefined), obj);
  }

  async function buildPolicyForm() {
    const form = $("pol-form");
    if (form.dataset.built) return;
    const cfg = await api("config").catch(() => null);
    if (!cfg) return;
    form.textContent = "";
    FIELDS.forEach(([path, label, type, step]) => {
      const wrap = el("div", "field");
      const lab = el("label", null, label);
      const input = el("input");
      input.type = type;
      input.step = String(step);
      input.dataset.path = path;
      const v = getPath(cfg, path);
      input.value = v === undefined || v === null ? "" : String(v);
      wrap.appendChild(lab);
      wrap.appendChild(input);
      form.appendChild(wrap);
    });
    const wrap = el("div", "field");
    wrap.appendChild(el("label", null, "包含皮肤温度"));
    const sel = el("select");
    sel.dataset.path = "policy.thermal.include_skin";
    sel.innerHTML = `<option value="false">否（仅 CPU 传感器）</option><option value="true">是</option>`;
    sel.value = String(getPath(cfg, "policy.thermal.include_skin") || false);
    wrap.appendChild(sel);
    form.appendChild(wrap);
    form.dataset.built = "1";
  }

  async function savePolicy() {
    const form = $("pol-form");
    const patch = {};
    form.querySelectorAll("[data-path]").forEach((input) => {
      const path = input.dataset.path;
      let value = input.value;
      if (input.type === "number") value = Number(value);
      if (value === "" || (typeof value === "number" && Number.isNaN(value))) return;
      if (value === "true") value = true;
      if (value === "false") value = false;
      const parts = path.split(".");
      let node = patch;
      parts.slice(0, -1).forEach((k) => {
        node[k] = node[k] || {};
        node = node[k];
      });
      node[parts[parts.length - 1]] = value;
    });
    const msg = $("pol-save-msg");
    msg.textContent = "保存中…";
    try {
      const res = await api("config", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(patch),
      });
      msg.textContent = res && res.message ? res.message : "已保存";
      form.dataset.built = "";
      await buildPolicyForm();
    } catch (e) {
      msg.textContent = `保存失败：${e.message}`;
    }
  }

  async function loadPolicy() {
    await buildPolicyForm();
    const p = await api("policy").catch(() => null);
    if (p) renderPolicy(p);
  }

  /* ---------------- safety ---------------- */
  function renderSafety(s) {
    $("s-writes").textContent = s.writes_allowed ? "允许写入" : "禁止写入";
    document.querySelectorAll("#s-mode button").forEach((b) => {
      b.classList.toggle("active", b.dataset.mode === s.mode);
    });
    const st = $("s-state");
    st.textContent = "";
    const t = (state.status && state.status.thermal) || {};
    kvRows(st, [
      ["模式", s.mode],
      ["配置模式", (state.status && state.status.configured_mode) || s.mode],
      ["温度可信度", t.trust || "unknown"],
      ["可信度说明", t.detail || "—"],
      ["Fail-Closed", s.thermal_fail_closed ? `是（${s.fail_closed_reason || ""}）` : "否"],
      ["Watchdog", s.watchdog],
      ["SAFE_MODE", s.safe_mode ? `是（${s.safe_reason}）` : "否"],
      ["本次写入", `成功 ${s.applied} / 失败 ${s.failed}`],
      ["冲突", (s.conflicts || []).length ? s.conflicts.join("；") : "无"],
      ["崩溃恢复", (s.recovered || []).length ? s.recovered.join("；") : "无需恢复"],
    ]);

    const cap = $("s-cap");
    cap.textContent = "";
    const c = s.capability || {};
    kvRows(cap, [
      ["mode", c.mode],
      ["writes_allowed", String(c.writes_allowed)],
      ["uclamp", c.uclamp ? `${c.uclamp.dir} (${c.uclamp.flavour})` : "不可用"],
      ["uclamp 当前", c.uclamp ? `min ${c.uclamp.current_min} / max ${c.uclamp.current_max}` : "—"],
      ["uclamp 尺度", c.uclamp ? `${c.uclamp.scale}（写满 = ${c.uclamp.profile_max_on_this_node}）` : "—"],
      ["global clamp", c.sched_util_clamp ? `${c.sched_util_clamp.min}/${c.sched_util_clamp.max}` : "不可用"],
      ["已释放节点", (c.released_nodes || []).join(", ") || "无"],
    ]);

    const nt = $("s-nodes");
    nt.textContent = "";
    nt.appendChild(thead(["节点", "所有者", "原始值", "最后写入", "状态"]));
    const nb = el("tbody");
    (s.nodes || []).forEach((n) => {
      const tr = el("tr");
      tr.appendChild(el("td", "mono", n.node));
      tr.appendChild(el("td", null, n.owner));
      tr.appendChild(el("td", "mono", n.original === null ? "—" : n.original));
      tr.appendChild(el("td", "mono", n.last_written === null ? "—" : n.last_written));
      tr.appendChild(el("td", n.released ? "bad" : "ok", n.released ? `已释放 ${n.note || ""}` : "持有"));
      nb.appendChild(tr);
    });
    nt.appendChild(nb);
    if (!(s.nodes || []).length) nt.appendChild(el("div", "empty", "尚未记录任何节点"));

    const jt = $("s-journal");
    jt.textContent = "";
    jt.appendChild(thead(["时间", "节点", "旧值", "新值", "状态", "结果"]));
    const jb = el("tbody");
    (s.journal || []).forEach((e) => {
      const tr = el("tr");
      tr.appendChild(el("td", "mono", new Date(e.ts).toLocaleTimeString()));
      tr.appendChild(el("td", "mono", e.node));
      tr.appendChild(el("td", "mono", e.old));
      tr.appendChild(el("td", "mono", e.new));
      tr.appendChild(el("td", null, e.state));
      tr.appendChild(el("td", e.result === "ok" ? "ok" : "warn", e.result));
      jb.appendChild(tr);
    });
    jt.appendChild(jb);
  }

  async function loadSafety() {
    const s = await api("safety").catch(() => null);
    if (s) renderSafety(s);
  }

  /* ---------------- logs ---------------- */
  async function loadLogs() {
    const level = $("log-level").value;
    const filter = $("log-filter").value.trim().toLowerCase();
    const rows = await api("logs?limit=400").catch(() => []);
    const t = $("log-table");
    t.textContent = "";
    t.appendChild(thead(["时间", "级别", "来源", "内容"]));
    const tb = el("tbody");
    const order = { debug: 0, info: 1, warn: 2, error: 3 };
    (rows || [])
      .filter((r) => order[r.level] >= order[level])
      .filter((r) => !filter || r.message.toLowerCase().includes(filter) || r.target.toLowerCase().includes(filter))
      .reverse()
      .forEach((r) => {
        const tr = el("tr");
        tr.appendChild(el("td", "mono", new Date(r.ts).toLocaleTimeString()));
        tr.appendChild(el("td", r.level === "error" ? "bad" : r.level === "warn" ? "warn" : null, r.level));
        tr.appendChild(el("td", "mono", r.target));
        tr.appendChild(el("td", null, r.message));
        tb.appendChild(tr);
      });
    t.appendChild(tb);
  }

  /* ---------------- actions ---------------- */
  async function action(name, extra) {
    if (state.busy) return;
    state.busy = true;
    try {
      await api("action", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(Object.assign({ action: name }, extra || {})),
      });
      setTimeout(poll, 400);
    } catch (e) {
      alert(`操作失败：${e.message}`);
    } finally {
      state.busy = false;
    }
  }

  function initActions() {
    $("p-now").addEventListener("click", () => action("predict"));
    $("p-reset").addEventListener("click", () => {
      if (confirm("重置预测模型？历史统计会清空。")) action("reset_model");
    });
    $("s-rollback").addEventListener("click", () => {
      if (confirm("立即把所有已写入的节点恢复为原始值？")) action("rollback");
    });
    $("s-reload").addEventListener("click", () => action("reload"));
    $("pol-save").addEventListener("click", savePolicy);
    $("pol-reload").addEventListener("click", async () => {
      $("pol-form").dataset.built = "";
      await buildPolicyForm();
      $("pol-save-msg").textContent = "已重新载入";
    });
    document.querySelectorAll("#s-mode button").forEach((b) => {
      b.addEventListener("click", () => {
        const mode = b.dataset.mode;
        action("mode", { value: mode });
      });
    });
    $("log-refresh").addEventListener("click", loadLogs);
    $("log-level").addEventListener("change", loadLogs);
    $("log-filter").addEventListener("input", () => {
      clearTimeout(window.__logTimer);
      window.__logTimer = setTimeout(loadLogs, 250);
    });
    $("log-export").addEventListener("click", async (event) => {
      event.preventDefault();
      try {
        const res = await request("export.csv");
        const url = URL.createObjectURL(await res.blob());
        const download = el("a");
        download.href = url;
        download.download = "neurosched-history.csv";
        download.click();
        setTimeout(() => URL.revokeObjectURL(url), 1000);
      } catch (error) {
        alert(`导出失败：${error.message}`);
      }
    });
  }

  function initAuth() {
    $("diagnostics-export").addEventListener("click", async () => {
      try {
        const report = await api("diagnostics");
        const url = URL.createObjectURL(new Blob([JSON.stringify(report, null, 2)], { type: "application/json" }));
        const download = el("a");
        download.href = url;
        download.download = `OryonPilot-diagnostics-${new Date().toISOString().slice(0, 10)}.json`;
        download.click();
        setTimeout(() => URL.revokeObjectURL(url), 1000);
        $("auth-message").textContent = "诊断已导出：不包含凭证、应用使用记录、模型或原始日志。";
      } catch (error) { $("auth-message").textContent = `导出失败：${error.message}`; }
    });
    $("auth-form").addEventListener("submit", (event) => {
      event.preventDefault();
      token = $("auth-token").value.trim();
      $("auth-token").value = "";
      $("auth-message").textContent = "连接中…";
      poll();
      refreshTab();
    });
    $("auth-clear").addEventListener("click", () => {
      token = "";
      window.location.reload();
    });
  }

  function thead(cols) {
    const t = el("thead");
    const tr = el("tr");
    cols.forEach((c) => tr.appendChild(el("th", null, c)));
    t.appendChild(tr);
    return t;
  }

  /* ---------------- boot ---------------- */
  document.addEventListener("DOMContentLoaded", () => {
    initTabs();
    initActions();
    initAuth();
    poll();
    setInterval(poll, 2000);
    window.addEventListener("resize", () => {
      if (state.tab === "dashboard") drawHistory();
    });
    document.addEventListener("visibilitychange", () => {
      if (!document.hidden) poll();
    });
  });
})();

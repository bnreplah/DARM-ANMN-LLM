// DARM-ANN node monitor: polling and drawing.
//
// This page is a read-only viewer. It makes GET requests to the archive
// nodes it is pointed at and draws what they report. It holds no key and
// never sends a write. It does not verify signatures itself: the archive
// node does that before serving anything, so the picture is as trustworthy
// as the archives you choose.
//
// Nothing an archive returns is ever inserted as HTML. Text goes through
// `textContent`, and the model has already reduced ids to 64 hex characters.
(function () {
  "use strict";
  const M = self.DarmModel;
  const POLL_MS = 10000;
  const TIMEOUT_MS = 8000;
  const MAX_BODY = 4 * 1024 * 1024;
  const $ = (id) => document.getElementById(id);

  let model = M.createModel();
  M.ensureObserver(model);
  const tokens = new Map(); // archive url -> read token, memory only
  const layout = new Map(); // node id -> {x, y, vx, vy}
  const pulses = []; // {key, t}
  let sim = null;
  let simHeight = 0;
  const reduceMotion = matchMedia("(prefers-reduced-motion: reduce)").matches;

  // ------------------------------------------------------------ fetching

  async function getJson(url, token) {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), TIMEOUT_MS);
    try {
      const response = await fetch(url, {
        method: "GET",
        mode: "cors",
        credentials: "omit",
        cache: "no-store",
        redirect: "error",
        referrerPolicy: "no-referrer",
        headers: token ? { Authorization: "Bearer " + token } : {},
        signal: controller.signal,
      });
      if (!response.ok) throw new Error("HTTP " + response.status);
      const text = await response.text();
      if (text.length > MAX_BODY) throw new Error("response too large");
      return JSON.parse(text);
    } finally {
      clearTimeout(timer);
    }
  }

  async function pollSource(source) {
    const token = tokens.get(source.url);
    try {
      M.applyInfo(model, source.url, await getJson(source.url + "/v1/info", token));
      // Catch up a bounded number of pages per poll.
      for (let page = 0; page < 4 && source.cursor < source.height; page += 1) {
        const url = source.url + "/v1/ledger/blocks?after=" + source.cursor + "&limit=32";
        const events = M.applyBlocks(model, source.url, await getJson(url, token));
        if (events.length === 0) break;
        for (const event of events) queuePulses(event);
      }
    } catch (error) {
      const reason = error && error.name === "AbortError" ? "timed out" : String(error.message || error);
      M.markError(model, source.url, reason);
    }
  }

  async function pollAll() {
    if (!model.simulated) {
      await Promise.all([...model.sources.values()].map(pollSource));
    }
    render();
  }

  function queuePulses(event) {
    if (reduceMotion) return;
    for (const key of event.pulses.slice(0, 48)) {
      pulses.push({ key, t: -Math.random() * 0.5 });
    }
    if (pulses.length > 600) pulses.splice(0, pulses.length - 600);
  }

  // ------------------------------------------------------------- panels

  function short(id) {
    return id.length === 64 ? id.slice(0, 8) + "…" + id.slice(-4) : id;
  }

  function cell(row, text, className) {
    const td = document.createElement("td");
    td.textContent = text;
    if (className) td.className = className;
    row.appendChild(td);
  }

  function render() {
    const s = M.summary(model);
    $("s-chain").textContent = s.chain || "-";
    $("s-height").textContent = s.reachable ? String(s.height) : "-";
    $("s-head").textContent = s.head ? short(s.head) : "-";
    $("s-nodes").textContent = String(s.meshNodes);
    $("s-validators").textContent = String(s.validators);
    $("s-quorum").textContent = s.quorum ? s.quorum.needed + " of " + s.quorum.total : "-";
    $("s-sources").textContent = s.reachable + " / " + s.sources;
    $("alone").hidden = !s.alone;
    $("simulated").hidden = !model.simulated;

    const list = $("source-list");
    list.replaceChildren();
    for (const source of model.sources.values()) {
      const item = document.createElement("li");
      const url = document.createElement("span");
      url.className = "url";
      url.textContent = source.url;
      const state = document.createElement("span");
      state.className = source.ok ? "ok" : "bad";
      state.textContent = source.ok ? "height " + source.height : source.error || "connecting";
      item.append(url, state);
      list.appendChild(item);
    }

    const rows = $("node-rows");
    rows.replaceChildren();
    const mesh = [...model.nodes.values()]
      .filter((n) => n.kind === "mesh")
      .sort((a, b) => Number(b.validator) - Number(a.validator) || b.lastHeight - a.lastHeight)
      .slice(0, 200);
    for (const node of mesh) {
      const row = document.createElement("tr");
      cell(row, node.id.slice(0, 6) + "\u2026" + node.id.slice(-4));
      cell(row, node.validator ? "validator" : "node", node.validator ? "validator" : "");
      cell(row, node.validator ? String(node.weight) : "-");
      cell(row, String(node.proposed));
      cell(row, String(node.attested));
      cell(row, String(node.authored));
      cell(row, node.lastHeight ? String(node.lastHeight) : "-");
      rows.appendChild(row);
    }
  }

  // ------------------------------------------------------------ drawing

  const canvas = $("graph");
  const ctx = canvas.getContext("2d");
  let width = 0;
  let height = 0;

  function resize() {
    const ratio = Math.min(devicePixelRatio || 1, 2);
    width = innerWidth;
    height = innerHeight;
    canvas.width = Math.round(width * ratio);
    canvas.height = Math.round(height * ratio);
    ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
  }

  /// The drawing area left of (or above) the panel.
  function stage() {
    const narrow = width <= 720;
    return narrow
      ? { cx: width / 2, cy: (height * 0.5) / 2 + 20, r: Math.min(width, height * 0.5) / 2 - 24 }
      : { cx: (width - 380) / 2, cy: height / 2, r: Math.min(width - 380, height) / 2 - 40 };
  }

  function place(id) {
    let p = layout.get(id);
    if (!p) {
      const st = stage();
      const angle = Math.random() * Math.PI * 2;
      const radius = id === M.OBSERVER_ID ? 0 : st.r * (0.3 + Math.random() * 0.6);
      p = { x: st.cx + Math.cos(angle) * radius, y: st.cy + Math.sin(angle) * radius, vx: 0, vy: 0 };
      layout.set(id, p);
    }
    return p;
  }

  function stepLayout() {
    const st = stage();
    const nodes = [...model.nodes.values()];
    for (const id of layout.keys()) if (!model.nodes.has(id)) layout.delete(id);
    const points = nodes.map((n) => place(n.id));
    const repel = Math.max(600, (st.r * st.r) / Math.max(nodes.length, 1));
    for (let i = 0; i < points.length; i += 1) {
      const a = points[i];
      for (let j = i + 1; j < points.length; j += 1) {
        const b = points[j];
        let dx = a.x - b.x;
        let dy = a.y - b.y;
        const d2 = dx * dx + dy * dy + 0.01;
        if (d2 > 90000) continue;
        const force = repel / d2;
        dx *= force / Math.sqrt(d2);
        dy *= force / Math.sqrt(d2);
        a.vx += dx; a.vy += dy; b.vx -= dx; b.vy -= dy;
      }
      a.vx += (st.cx - a.x) * 0.0015;
      a.vy += (st.cy - a.y) * 0.0015;
    }
    for (const link of model.links.values()) {
      const a = layout.get(link.from);
      const b = layout.get(link.to);
      if (!a || !b) continue;
      const dx = b.x - a.x;
      const dy = b.y - a.y;
      const d = Math.sqrt(dx * dx + dy * dy) + 0.01;
      const rest = link.kind === "read" ? 70 : 110;
      const pull = ((d - rest) / d) * 0.004;
      a.vx += dx * pull; a.vy += dy * pull; b.vx -= dx * pull; b.vy -= dy * pull;
    }
    for (const p of points) {
      p.vx *= 0.86; p.vy *= 0.86;
      p.x += Math.max(-6, Math.min(6, p.vx));
      p.y += Math.max(-6, Math.min(6, p.vy));
      const dx = p.x - st.cx;
      const dy = p.y - st.cy;
      const d = Math.sqrt(dx * dx + dy * dy);
      if (d > st.r) { p.x = st.cx + (dx / d) * st.r; p.y = st.cy + (dy / d) * st.r; }
    }
  }

  const COLORS = { observer: "#f4f8ff", archive: "#ffc857", validator: "#35e0ff", mesh: "#5f7bb0", bad: "#ff6b7a" };

  function draw(now) {
    ctx.clearRect(0, 0, width, height);
    ctx.lineWidth = 1;
    for (const link of model.links.values()) {
      const a = layout.get(link.from);
      const b = layout.get(link.to);
      if (!a || !b) continue;
      ctx.strokeStyle = link.kind === "read" ? "rgba(244,248,255,0.28)"
        : link.kind === "ledger" ? "rgba(255,200,87,0.10)"
        : "rgba(53,224,255," + Math.min(0.1 + link.hits * 0.02, 0.4) + ")";
      ctx.setLineDash(link.kind === "read" ? [3, 4] : []);
      ctx.beginPath(); ctx.moveTo(a.x, a.y); ctx.lineTo(b.x, b.y); ctx.stroke();
    }
    ctx.setLineDash([]);
    for (let i = pulses.length - 1; i >= 0; i -= 1) {
      const pulse = pulses[i];
      pulse.t += 0.018;
      const link = model.links.get(pulse.key);
      const a = link && layout.get(link.from);
      const b = link && layout.get(link.to);
      if (pulse.t >= 1 || !a || !b) { pulses.splice(i, 1); continue; }
      if (pulse.t < 0) continue;
      ctx.fillStyle = "#ff4fd8";
      ctx.globalAlpha = 1 - pulse.t * 0.6;
      ctx.beginPath();
      ctx.arc(a.x + (b.x - a.x) * pulse.t, a.y + (b.y - a.y) * pulse.t, 2.2, 0, Math.PI * 2);
      ctx.fill();
    }
    ctx.globalAlpha = 1;
    const beat = reduceMotion ? 0 : Math.sin(now / 900);
    for (const node of model.nodes.values()) {
      const p = layout.get(node.id);
      if (!p) continue;
      if (node.kind === "observer") {
        ctx.strokeStyle = COLORS.observer;
        ctx.lineWidth = 1.5;
        ctx.beginPath(); ctx.arc(p.x, p.y, 7, 0, Math.PI * 2); ctx.stroke();
        ctx.globalAlpha = 0.25 + 0.15 * beat;
        ctx.beginPath(); ctx.arc(p.x, p.y, 12 + 2 * beat, 0, Math.PI * 2); ctx.stroke();
        ctx.globalAlpha = 1;
        ctx.lineWidth = 1;
      } else {
        const color = !node.ok ? COLORS.bad : node.kind === "archive" ? COLORS.archive
          : node.validator ? COLORS.validator : COLORS.mesh;
        const radius = node.kind === "archive" ? 6 : node.validator ? 5 : 3.2;
        ctx.fillStyle = color;
        ctx.shadowColor = color;
        ctx.shadowBlur = node.validator || node.kind === "archive" ? 10 : 0;
        ctx.beginPath(); ctx.arc(p.x, p.y, radius, 0, Math.PI * 2); ctx.fill();
        ctx.shadowBlur = 0;
      }
      if (node.kind !== "mesh" || (node.validator && model.nodes.size <= 40)) {
        ctx.fillStyle = "rgba(207,227,255,0.8)";
        ctx.font = "11px ui-monospace, Menlo, Consolas, monospace";
        ctx.fillText(node.label || short(node.id), p.x + 10, p.y + 4);
      }
    }
  }

  function frame(now) {
    stepLayout();
    draw(now);
    requestAnimationFrame(frame);
  }

  // ------------------------------------------------------------ controls

  function showFormError(text) {
    $("form-error").textContent = text;
    $("form-error").hidden = !text;
  }

  $("add-source").addEventListener("submit", (event) => {
    event.preventDefault();
    if (model.simulated) setSimulated(false);
    const url = M.normalizeSource($("source-url").value);
    if (!url) return showFormError("Enter an http(s) URL with no query, fragment or credentials.");
    if (location.protocol === "https:" && url.startsWith("http://") && !/^http:\/\/(localhost|127\.0\.0\.1)(:|$)/.test(url)) {
      return showFormError("A page served over https can only read https archives (or localhost).");
    }
    if (!M.addSource(model, url)) return showFormError("That archive is already listed, or the list is full.");
    const token = $("read-token").value;
    if (token) tokens.set(url, token);
    $("source-url").value = "";
    $("read-token").value = "";
    showFormError("");
    pollAll();
  });

  let simTimer = 0;
  function setSimulated(on) {
    clearInterval(simTimer);
    const real = on ? [] : savedSources;
    if (on) savedSources = [...model.sources.keys()];
    model = M.createModel();
    M.ensureObserver(model);
    layout.clear();
    pulses.length = 0;
    if (on) {
      sim = M.simulate(model, 28);
      simHeight = 0;
      simTimer = setInterval(() => {
        simHeight += 1;
        for (const event of M.simulateBlock(model, sim, simHeight, Math.random)) queuePulses(event);
        render();
      }, 1500);
    } else {
      for (const url of real) M.addSource(model, url);
    }
    $("toggle-sim").setAttribute("aria-pressed", String(on));
    $("toggle-sim").textContent = on ? "Stop simulation" : "Preview with simulated nodes";
    pollAll();
  }
  let savedSources = [];
  $("toggle-sim").addEventListener("click", () => setSimulated(!model.simulated));

  // -------------------------------------------------------------- start

  async function loadConfig() {
    const fromQuery = new URLSearchParams(location.search).getAll("node");
    let fromFile = [];
    try {
      const config = await getJson("config.json");
      if (config && Array.isArray(config.nodes)) fromFile = config.nodes;
    } catch (_) {
      // No config.json is fine: the monitor starts alone.
    }
    for (const url of [...fromFile, ...fromQuery].slice(0, 16)) M.addSource(model, url);
  }

  addEventListener("resize", resize);
  resize();
  render();
  requestAnimationFrame(frame);
  loadConfig().then(() => {
    // `?demo=1` opens straight into the labelled simulation.
    if (new URLSearchParams(location.search).get("demo") === "1") setSimulated(true);
    else pollAll();
    setInterval(pollAll, POLL_MS);
  });
})();

const ROW_H = 26;
const state = {
  q: "",
  fromNs: null,
  toNs: null,
  tab: "logs",
  live: false,
  es: null,
  total: 0,
  buckets: [],
  selected: null,
  sort: "count",
};

const qInput = document.getElementById("q");
const scroller = document.getElementById("scroller");
const spacer = document.getElementById("spacer");
const rowsEl = document.getElementById("rows");
const hist = document.getElementById("hist");
const ctx = hist.getContext("2d");

qInput.addEventListener("input", () => {
  state.q = qInput.value.trim();
  scheduleReload();
});
document.getElementById("qform").addEventListener("submit", (e) => {
  e.preventDefault();
  reload();
});
document.getElementById("tab-logs").onclick = () => setTab("logs");
document.getElementById("tab-patterns").onclick = () => setTab("patterns");
document.getElementById("live").onclick = toggleLive;
document.getElementById("clear-brush").onclick = () => {
  state.fromNs = null;
  state.toNs = null;
  document.getElementById("clear-brush").hidden = true;
  drawHist();
  loadWindow(true);
};
window.addEventListener("keydown", (e) => {
  if (e.key === "/" && document.activeElement !== qInput) {
    e.preventDefault();
    qInput.focus();
  }
  if (e.key === "Escape") {
    document.getElementById("drawer").hidden = true;
  }
});
scroller.addEventListener("scroll", () => renderVisible());

let reloadTimer = 0;
function scheduleReload() {
  clearTimeout(reloadTimer);
  reloadTimer = setTimeout(reload, 180);
}

function setTab(tab) {
  state.tab = tab;
  document.getElementById("tab-logs").classList.toggle("on", tab === "logs");
  document.getElementById("tab-patterns").classList.toggle("on", tab === "patterns");
  document.getElementById("logs-view").hidden = tab !== "logs";
  document.getElementById("patterns-view").hidden = tab !== "patterns";
  if (tab === "patterns") loadPatterns();
}

async function reload() {
  state.q = qInput.value.trim();
  await Promise.all([loadSummary(), loadFacets(), loadHist(), loadWindow(true), loadBookmarks()]);
  if (state.tab === "patterns") await loadPatterns();
}

async function loadSummary() {
  const data = await getJSON("/api/summary?q=" + encodeURIComponent(state.q));
  const levels = data.levels || {};
  const err = (levels.error || 0) + (levels.fatal || 0);
  const span = data.tmin_ns && data.tmax_ns
    ? `${fmtTs(data.tmin_ns)} → ${fmtTs(data.tmax_ns)}`
    : "no timestamps";
  const ratio = data.payload_bytes
    ? `index ${(100 * data.index_bytes / data.payload_bytes).toFixed(2)}% of payload`
    : "";
  document.getElementById("stats").innerHTML =
    `<span><b>${data.total.toLocaleString()}</b> events</span>` +
    `<span>${span}</span>` +
    `<span><b>${err.toLocaleString()}</b> errors</span>` +
    `<span>${(data.sources || []).slice(0, 4).join(", ")}</span>` +
    `<span>${ratio}</span>` +
    `<span class="marks" id="marks"></span>`;
}

async function loadFacets() {
  const data = await getJSON("/api/facets?q=" + encodeURIComponent(state.q));
  const root = document.getElementById("facets");
  root.innerHTML = "";
  for (const facet of data.facets || []) {
    const box = document.createElement("div");
    box.className = "facet";
    const h = document.createElement("h3");
    h.textContent = facet.key;
    box.appendChild(h);
    for (const pair of facet.values) {
      const value = pair[0];
      const count = pair[1];
      const row = document.createElement("div");
      row.className = "facet-row";
      const name = document.createElement("span");
      name.className = "name";
      name.textContent = value;
      name.title = value;
      const c = document.createElement("span");
      c.className = "count";
      c.textContent = count;
      const inc = document.createElement("button");
      inc.textContent = "+";
      inc.title = "include";
      inc.onclick = () => addClause(facetClause(facet.key, value, false));
      const exc = document.createElement("button");
      exc.textContent = "–";
      exc.title = "exclude";
      exc.onclick = () => addClause(facetClause(facet.key, value, true));
      row.append(name, c, inc, exc);
      box.appendChild(row);
    }
    root.appendChild(box);
  }
  if (!root.children.length) {
    root.innerHTML = `<div class="empty">No facets yet.</div>`;
  }
}

function facetClause(key, value, neg) {
  const safe = /[\s:]/.test(value) ? `"${value.replaceAll('"', '\\"')}"` : value;
  if (key === "level" || key === "service" || key === "source" || key === "status" || key === "host") {
    return `${neg ? "-" : ""}${key}:${safe}`;
  }
  return `${neg ? "-" : ""}${key}:${safe}`;
}

function addClause(clause) {
  const cur = qInput.value.trim();
  if (cur.split(/\s+/).includes(clause)) return;
  qInput.value = cur ? `${cur} ${clause}` : clause;
  reload();
}

async function loadHist() {
  const data = await getJSON("/api/histogram?q=" + encodeURIComponent(state.q) + "&buckets=48");
  state.buckets = data.buckets || [];
  drawHist();
}

function drawHist() {
  const dpr = window.devicePixelRatio || 1;
  const w = hist.clientWidth || hist.parentElement.clientWidth || 600;
  const h = 96;
  hist.width = Math.floor(w * dpr);
  hist.height = Math.floor(h * dpr);
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
  const buckets = state.buckets;
  if (!buckets.length) return;
  const max = Math.max(1, ...buckets.map((b) => b.info + b.warn + b.error));
  const bw = w / buckets.length;
  buckets.forEach((b, i) => {
    let y = h - 8;
    const segs = [
      [b.info, "#8eb4c9"],
      [b.warn, "#e4a23a"],
      [b.error, "#e15d45"],
    ];
    for (const [count, color] of segs) {
      const bh = (count / max) * (h - 16);
      y -= bh;
      ctx.fillStyle = color;
      ctx.fillRect(i * bw + 1, y, Math.max(1, bw - 2), bh);
    }
    if (state.fromNs != null && state.toNs != null) {
      const mid = (b.start_ns + b.end_ns) / 2;
      if (mid < state.fromNs || mid > state.toNs) {
        ctx.fillStyle = "rgba(12,14,10,0.55)";
        ctx.fillRect(i * bw, 0, bw, h);
      }
    }
  });
}

let brushAnchor = null;
hist.addEventListener("mousedown", (e) => {
  brushAnchor = bucketAt(e);
  applyBrush(brushAnchor, brushAnchor);
});
hist.addEventListener("mousemove", (e) => {
  if (brushAnchor == null || e.buttons !== 1) return;
  applyBrush(brushAnchor, bucketAt(e));
});
window.addEventListener("mouseup", () => {
  if (brushAnchor != null) {
    brushAnchor = null;
    loadWindow(true);
  }
});

function bucketAt(e) {
  const rect = hist.getBoundingClientRect();
  const x = Math.min(Math.max(0, e.clientX - rect.left), rect.width - 1);
  const i = Math.floor((x / rect.width) * state.buckets.length);
  return Math.max(0, Math.min(state.buckets.length - 1, i));
}

function applyBrush(a, b) {
  const lo = Math.min(a, b);
  const hi = Math.max(a, b);
  const start = state.buckets[lo];
  const end = state.buckets[hi];
  if (!start || !end) return;
  state.fromNs = start.start_ns;
  state.toNs = end.end_ns;
  document.getElementById("clear-brush").hidden = false;
  drawHist();
}

async function loadWindow(reset) {
  const offset = reset ? 0 : firstVisible();
  const limit = 180;
  const params = new URLSearchParams({
    q: state.q,
    offset: String(Math.max(0, offset)),
    limit: String(limit),
  });
  if (state.fromNs != null) params.set("from_ns", String(state.fromNs));
  if (state.toNs != null) params.set("to_ns", String(state.toNs));
  const data = await getJSON("/api/query?" + params.toString());
  state.total = data.total || 0;
  state.pageOffset = data.offset || 0;
  state.page = data.events || [];
  spacer.style.height = state.total * ROW_H + "px";
  if (reset) scroller.scrollTop = 0;
  renderVisible();
  if (!state.total) {
    rowsEl.innerHTML = `<div class="empty">No lines match. The index is empty, or the query filtered everything out.</div>`;
  }
}

function firstVisible() {
  return Math.max(0, Math.floor(scroller.scrollTop / ROW_H) - 20);
}

let windowTimer = 0;
function renderVisible() {
  const start = Math.floor(scroller.scrollTop / ROW_H);
  const view = Math.ceil(scroller.clientHeight / ROW_H) + 8;
  const pageStart = state.pageOffset || 0;
  const pageEnd = pageStart + (state.page ? state.page.length : 0);
  if (state.total && (start < pageStart || start + view > pageEnd)) {
    clearTimeout(windowTimer);
    windowTimer = setTimeout(() => loadWindow(false), 60);
  }
  if (!state.page) return;
  rowsEl.innerHTML = "";
  const from = Math.max(start, pageStart);
  const to = Math.min(start + view, pageEnd);
  for (let i = from; i < to; i++) {
    const ev = state.page[i - pageStart];
    if (!ev) continue;
    const row = document.createElement("div");
    row.className = "row" + (state.selected === ev.id ? " sel" : "");
    row.style.top = i * ROW_H + "px";
    row.innerHTML =
      `<span>${ev.ts ? ev.ts.replace("T", " ").replace("Z", "") : "—"}</span>` +
      `<span class="lv-${ev.level}">${ev.level}</span>` +
      `<span>${escapeHtml(ev.service || ev.source || "")}</span>` +
      `<span>${escapeHtml(ev.message || "")}</span>`;
    row.onclick = () => openDrawer(ev);
    rowsEl.appendChild(row);
  }
}

async function openDrawer(ev) {
  state.selected = ev.id;
  renderVisible();
  const data = await getJSON(`/api/context?id=${ev.id}&n=20`);
  const drawer = document.getElementById("drawer");
  drawer.hidden = false;
  const corr = correlationId(data.event.facets || {});
  drawer.innerHTML = `
    <div class="drawer-bar">
      <button type="button" id="ctx-btn">Show context (±20 lines)</button>
      <button type="button" id="corr-btn" ${corr ? "" : "disabled"}>Correlate ${corr ? escapeHtml(corr) : ""}</button>
      <button type="button" id="close-drawer">Close</button>
    </div>
    <h2>${escapeHtml(data.event.message || "")}</h2>
    <div class="kv" id="kv"></div>
    <pre class="stack" id="stack"></pre>
    <pre class="ctx" id="ctx" hidden></pre>
    <div id="trace"></div>`;
  const kv = document.getElementById("kv");
  const facets = data.event.facets || {};
  for (const key of Object.keys(facets)) {
    kv.insertAdjacentHTML("beforeend", `<b>${escapeHtml(key)}</b><span>${escapeHtml(facets[key])}</span>`);
  }
  document.getElementById("stack").textContent = data.stack || data.event_text || "";
  document.getElementById("close-drawer").onclick = () => { drawer.hidden = true; };
  document.getElementById("ctx-btn").onclick = () => {
    const pre = document.getElementById("ctx");
    pre.hidden = false;
    const before = (data.before || []).join("\n");
    const after = (data.after || []).join("\n");
    pre.textContent = `${before}\n—— event ——\n${data.event_text || ""}\n—— after ——\n${after}`;
  };
  document.getElementById("corr-btn").onclick = async () => {
    if (!corr) return;
    const trace = await getJSON("/api/trace?id=" + encodeURIComponent(corr));
    const host = document.getElementById("trace");
    host.innerHTML = `<h3>${trace.events.length} events for ${escapeHtml(corr)}</h3>`;
    for (const ev of trace.events) {
      const div = document.createElement("div");
      div.className = "row";
      div.style.position = "relative";
      div.innerHTML = `<span>${ev.ts || ""}</span><span class="lv-${ev.level}">${ev.level}</span><span>${escapeHtml(ev.service || "")}</span><span>${escapeHtml(ev.message || "")}</span>`;
      host.appendChild(div);
    }
  };
}

function correlationId(facets) {
  for (const key of ["trace_id", "request_id", "req_id", "correlation_id", "span_id", "traceId", "requestId"]) {
    if (facets[key]) return facets[key];
  }
  return "";
}

async function loadPatterns() {
  const data = await getJSON(`/api/patterns?q=${encodeURIComponent(state.q)}&sort=${state.sort}`);
  const root = document.getElementById("patterns-view");
  root.innerHTML = `<div class="sortbar">Sort
    <button type="button" id="sort-count" class="${state.sort === "count" ? "on" : ""}">count</button>
    <button type="button" id="sort-spike" class="${state.sort === "spike" ? "on" : ""}">spike</button>
  </div>`;
  document.getElementById("sort-count").onclick = () => { state.sort = "count"; loadPatterns(); };
  document.getElementById("sort-spike").onclick = () => { state.sort = "spike"; loadPatterns(); };
  for (const pat of data.patterns || []) {
    const el = document.createElement("div");
    el.className = "pat";
    const levels = pat.levels || {};
    const total = Object.values(levels).reduce((a, b) => a + b, 0) || 1;
    const info = ((levels.info || 0) + (levels.debug || 0) + (levels.trace || 0) + (levels.unknown || 0)) / total;
    const warn = (levels.warn || 0) / total;
    const err = ((levels.error || 0) + (levels.fatal || 0)) / total;
    el.innerHTML = `
      <div>
        <div class="count">${pat.count}</div>
        <div class="sev"><i style="width:${info * 100}%;background:#8eb4c9"></i><i style="width:${warn * 100}%;background:#e4a23a"></i><i style="width:${err * 100}%;background:#e15d45"></i></div>
      </div>
      <div>
        <div class="tpl"></div>
        <div class="meta">${pat.spike ? `<span class="spike">${escapeHtml(pat.spike)}</span>` : ""}</div>
      </div>
      <canvas width="140" height="36"></canvas>`;
    el.querySelector(".tpl").textContent = pat.template;
    drawSpark(el.querySelector("canvas"), pat.spark || []);
    el.onclick = () => {
      qInput.value = `template:${pat.id}`;
      setTab("logs");
      reload();
    };
    root.appendChild(el);
  }
  if (!(data.patterns || []).length) {
    root.insertAdjacentHTML("beforeend", `<div class="empty">No templates.</div>`);
  }
}

function drawSpark(canvas, spark) {
  const c = canvas.getContext("2d");
  const w = canvas.width;
  const h = canvas.height;
  c.clearRect(0, 0, w, h);
  const max = Math.max(1, ...spark);
  const bw = w / Math.max(1, spark.length);
  c.fillStyle = "#e4a23a";
  spark.forEach((v, i) => {
    const bh = (v / max) * (h - 4);
    c.fillRect(i * bw, h - bh, Math.max(1, bw - 1), bh);
  });
}

async function loadBookmarks() {
  const marks = document.getElementById("marks");
  if (!marks) return;
  const rows = await getJSON("/api/bookmarks");
  marks.innerHTML = "";
  for (const b of rows) {
    const btn = document.createElement("button");
    btn.textContent = b.label;
    btn.onclick = () => { qInput.value = b.query; reload(); };
    marks.appendChild(btn);
  }
  const save = document.createElement("button");
  save.textContent = "Save query";
  save.onclick = async () => {
    const label = prompt("Bookmark label", state.q.slice(0, 24) || "view");
    if (!label) return;
    await fetch("/api/bookmarks", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ label, query: qInput.value.trim() }),
    });
    loadSummary().then(loadBookmarks);
  };
  marks.appendChild(save);
}

function toggleLive() {
  state.live = !state.live;
  document.getElementById("live").classList.toggle("on", state.live);
  if (state.es) {
    state.es.close();
    state.es = null;
  }
  if (!state.live) return;
  state.es = new EventSource("/api/stream");
  state.es.onmessage = () => {
    if (!state.live) return;
    loadSummary();
    loadHist();
    if (scroller.scrollTop < 40) loadWindow(true);
  };
}

function fmtTs(ns) {
  if (!ns) return "";
  const ms = Math.floor(ns / 1e6);
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return "";
  return d.toISOString().replace(".000Z", "Z");
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}

async function getJSON(url) {
  const res = await fetch(url);
  if (!res.ok) {
    const text = await res.text();
    throw new Error(text);
  }
  return res.json();
}

window.addEventListener("resize", drawHist);
reload().catch((err) => {
  document.getElementById("stats").textContent = String(err);
});

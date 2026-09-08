"use strict";
const $ = (id) => document.getElementById(id);
const feed = $("feed"), input = $("input"), sendBtn = $("send"), stopBtn = $("stop"), statusEl = $("status"), connEl = $("conn");

// ── session 管理（localStorage；条目 {id,title}，旧格式 string[] 自动迁移）──
// title 为空时侧栏显示 id；发送首条消息后按消息内容自动命名；手动重命名后不再覆盖（清空标题可恢复自动命名）。
// W17 迭代六：key 按「实例工作区」隔离（host 注入 window.RA_WS）——多实例端口动态分配、
// 可被不同工作区的实例复用，按端口（源）存列表会跨工作区串显；绑工作区后实例复活换端口也不丢。
const WS_KEY = String(window.RA_WS ?? "");
const SKEY = "ra.sessions#" + WS_KEY, CKEY = "ra.session#" + WS_KEY;
let session = localStorage.getItem(CKEY) || "web";
function loadSessions() {
  try {
    const arr = JSON.parse(localStorage.getItem(SKEY) || "[]");
    return arr.map((s) => (typeof s === "string" ? { id: s, title: "" } : s)).filter((s) => s && s.id);
  } catch { return []; }
}
function saveSessions(arr) { localStorage.setItem(SKEY, JSON.stringify(arr)); }
function sessTitle(s) { return s.title || s.id; }
function touchSession(id) {
  const arr = loadSessions();
  const i = arr.findIndex((s) => s.id === id);
  if (i >= 0) { const [it] = arr.splice(i, 1); arr.unshift(it); }
  else arr.unshift({ id, title: "" });
  saveSessions(arr.slice(0, 30));
}
function refreshSessionTag() {
  const cur = loadSessions().find((s) => s.id === session);
  const tag = $("session-tag");
  tag.textContent = "session: " + (cur && cur.title ? cur.title : session);
  tag.title = "会话 ID: " + session;
}
function renderSessions() {
  const box = $("sessions"); box.innerHTML = "";
  const arr = loadSessions();
  if (!arr.length) { box.innerHTML = '<div class="empty">暂无历史会话</div>'; return; }
  for (const s of arr) {
    const el = document.createElement("div");
    el.className = "sess" + (s.id === session ? " active" : "");
    el.title = "会话 ID: " + s.id;
    el.innerHTML = `<span class="dot"></span><span class="name">${esc(sessTitle(s))}</span>`;
    el.onclick = () => switchSession(s.id);
    const ops = document.createElement("span");
    ops.className = "ops";
    const ren = document.createElement("span");
    ren.className = "ren"; ren.textContent = "✎"; ren.title = "重命名";
    ren.onclick = (e) => { e.stopPropagation(); renameSession(s.id); };
    const del = document.createElement("span");
    del.className = "del"; del.textContent = "×"; del.title = "删除会话";
    del.onclick = (e) => { e.stopPropagation(); delSession(s.id); };
    ops.append(ren, del);
    el.appendChild(ops);
    box.appendChild(el);
  }
}
function renameSession(id) {
  const cur = loadSessions().find((s) => s.id === id);
  const v = prompt("重命名会话（清空并确定则恢复自动命名）", cur ? cur.title || "" : "");
  if (v === null) return;
  const arr = loadSessions();
  const it = arr.find((s) => s.id === id);
  if (!it) return;
  it.title = v.trim();
  saveSessions(arr); renderSessions(); refreshSessionTag();
}
function autoName(id, text) {
  const arr = loadSessions();
  const it = arr.find((s) => s.id === id);
  if (!it || it.title) return; // 已手动命名（或已命名过）则不覆盖
  const t = (text || "").replace(/\s+/g, " ").trim().slice(0, 24);
  if (!t) return;
  it.title = t;
  saveSessions(arr); renderSessions(); refreshSessionTag();
}
function delSession(id) {
  if (!confirm("删除会话 " + id + "？（仅清本地记录，服务端消息日志保留）")) return;
  let arr = loadSessions().filter((x) => x.id !== id);
  saveSessions(arr);
  if (session === id) {
    session = arr.length ? arr[0].id : "web";
    localStorage.setItem(CKEY, session);
  }
  refreshSessionTag();
  renderSessions(); connect();
}
function switchSession(id) {
  if (id === session) return;
  session = id; localStorage.setItem(CKEY, id); touchSession(id);
  refreshSessionTag();
  renderSessions(); connect();
}
refreshSessionTag(); touchSession(session); renderSessions();
$("toggle").onclick = () => $("sidebar").classList.toggle("collapsed");
$("newchat").onclick = () => {
  const v = prompt("新会话 ID（可自定义易记名称；留空自动生成）", "s-" + Date.now().toString(36));
  if (v === null) return;
  switchSession(v.trim() || "s-" + Date.now().toString(36));
};

// 旧命名空间（按端口，key 无工作区后缀）一次性迁移：多实例端口被不同工作区复用后，
// 旧列表可能混着别的实例的会话——逐个 /api/session-exists 校验，只收编本实例 traces
// 里真实存在的；其余属于别的工作区实例（切到对应界面可见），不在本实例重复显示。
// 迁移完成前旧 key 保留（刷新可重试）；完成后删除，永不再问。
(async function migrateLegacySessions() {
  if (!WS_KEY) return;
  if (loadSessions().length) { localStorage.removeItem("ra.sessions"); localStorage.removeItem("ra.session"); return; }
  let raw;
  try { raw = JSON.parse(localStorage.getItem("ra.sessions") || "[]"); } catch { raw = []; }
  if (!Array.isArray(raw) || !raw.length) { localStorage.removeItem("ra.sessions"); return; }
  const kept = [];
  for (const s of raw) {
    const id = typeof s === "string" ? s : s && s.id;
    if (!id) continue;
    try {
      const r = await fetch(`/api/session-exists?session=${encodeURIComponent(id)}`);
      const v = await r.json();
      if (v && v.exists) kept.push(typeof s === "string" ? { id, title: "" } : s);
    } catch { return; } // 网络异常：整批下次再试
  }
  if (kept.length) {
    localStorage.setItem(SKEY, JSON.stringify(kept.slice(0, 30)));
    const oldCur = localStorage.getItem("ra.session");
    if (oldCur && kept.some((s) => s.id === oldCur)) localStorage.setItem(CKEY, oldCur);
    renderSessions(); refreshSessionTag();
  }
  localStorage.removeItem("ra.sessions");
})();

function esc(s) { const d = document.createElement("div"); d.textContent = s ?? ""; return d.innerHTML; }

// ── markdown 渲染（先转义，再还原受控标签；代码块带语言标签+复制）──
function md(src) {
  let text = esc(src);
  const blocks = [];
  text = text.replace(/```(\w*)\n([\s\S]*?)```/g, (_, lang, body) => {
    const i = blocks.length;
    blocks.push({ lang: lang || "text", body: body.replace(/\n$/, "") });
    return `\u0000B${i}\u0000`;
  });
  // 表格：| a | b | 行 + |---|---| 分隔行 + 数据行。在行级规则前整块替换，
  // 单元格内容仍走后续 inline 规则（bold/code/link），语义不丢。
  text = text.replace(/(?:^\|.*\|$\n?)+/gm, (block) => {
    const rows = block.trim().split("\n").map((l) => l.trim());
    if (rows.length < 2 || !/^\|?[\s:|-]+\|?$/.test(rows[1]) || !rows[1].includes("-")) return block;
    const cells = (l) => l.replace(/^\|/, "").replace(/\|$/, "").split("|").map((c) => c.trim());
    const th = cells(rows[0]).map((c) => `<th>${c}</th>`).join("");
    const trs = rows.slice(2).map((l) => "<tr>" + cells(l).map((c) => `<td>${c}</td>`).join("") + "</tr>").join("");
    return `<table><thead><tr>${th}</tr></thead><tbody>${trs}</tbody></table>\n`;
  });
  text = text
    .replace(/^###### (.*)$/gm, "<h3>$1</h3>")
    .replace(/^##### (.*)$/gm, "<h3>$1</h3>")
    .replace(/^#### (.*)$/gm, "<h3>$1</h3>")
    .replace(/^### (.*)$/gm, "<h3>$1</h3>")
    .replace(/^## (.*)$/gm, "<h2>$1</h2>")
    .replace(/^# (.*)$/gm, "<h1>$1</h1>")
    .replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>")
    .replace(/\*([^*\n]+)\*/g, "<em>$1</em>")
    .replace(/`([^`\n]+)`/g, "<code>$1</code>")
    .replace(/\[([^\]]+)\]\((https?:[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>')
    .replace(/^&gt; ?(.*)$/gm, "<blockquote>$1</blockquote>");
  const lines = text.split("\n");
  let html = "", list = null, para = [];
  const flushPara = () => { if (para.length) { html += `<p>${para.join("<br>")}</p>`; para = []; } };
  const flushList = () => { if (list) { html += `</${list}>`; list = null; } };
  for (const ln of lines) {
    const t = ln.trim();
    let m;
    if (!t) { flushPara(); flushList(); continue; }
    if (/^\u0000B\d+\u0000$/.test(t)) { flushPara(); flushList(); html += t; continue; }
    if ((m = t.match(/^[-*] (.*)$/))) { flushPara(); if (list !== "ul") { flushList(); html += "<ul>"; list = "ul"; } html += `<li>${m[1]}</li>`; continue; }
    if ((m = t.match(/^\d+[.)] (.*)$/))) { flushPara(); if (list !== "ol") { flushList(); html += "<ol>"; list = "ol"; } html += `<li>${m[1]}</li>`; continue; }
    if (/^<(h\d|blockquote|ul|ol|table)/.test(t)) { flushPara(); flushList(); html += t; continue; }
    flushList(); para.push(t);
  }
  flushPara(); flushList();
  return html.replace(/\u0000B(\d+)\u0000/g, (_, i) => {
    const b = blocks[+i];
    return `<pre class="code"><div class="cb-head"><span class="cb-lang">${esc(b.lang)}</span><button class="cb-copy">复制</button></div><pre>${esc(b.body)}</pre></pre>`;
  });
}

// 代码块复制（事件委托）
feed.addEventListener("click", (e) => {
  const btn = e.target.closest(".cb-copy");
  if (btn) {
    const pre = btn.closest("pre.code")?.querySelector("pre");
    if (pre) navigator.clipboard.writeText(pre.textContent).then(() => { const o = btn.textContent; btn.textContent = "已复制"; setTimeout(() => (btn.textContent = o), 1200); });
  }
});

function el(html) { const d = document.createElement("div"); d.innerHTML = html; return d.firstElementChild; }
function scroll() { const s = $("stream"); s.scrollTop = s.scrollHeight; }
// 页面级跟随（R12）：事件流增长时 #stream 贴底滚动；用户上滚即暂停，距底 <64px 视为
// 回到底部自动恢复。scrollFeed 供事件驱动路径调用（过程框内滑动、产物/来源卡追加、
// 正文流式），与 scroll()（强制置底，用于发送消息等用户主动动作）区分。
let feedPinned = true;
$("stream").addEventListener("scroll", () => {
  const s = $("stream");
  feedPinned = s.scrollHeight - s.scrollTop - s.clientHeight < 64;
}, { passive: true });
function scrollFeed() { if (feedPinned) scroll(); }
function setStatus(t) { statusEl.textContent = t || ""; }


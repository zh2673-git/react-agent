"use strict";
const $ = (id) => document.getElementById(id);
const feed = $("feed"), input = $("input"), sendBtn = $("send"), stopBtn = $("stop"), statusEl = $("status"), connEl = $("conn");

// ── 单一会话（单会话形态）──
// 用户面对 agent 永远只有一个会话框：不再有会话列表、新建对话、重命名/删除。
// 历史靠「一份连续 trace + SSE 游标续传」自然累积，项目分类由 agent 自己在工作区完成
// （左侧看板才是分类视图，见 app-board.js）。旧的多会话 key 一次性清理，避免残留数据
// 在新形态下变成幽灵状态。
const WS_KEY = String(window.RA_WS ?? "");
const CKEY = "ra.session#" + WS_KEY;
const SESSION_ID = "main";
// 兼容层：旧代码（app-events/q-session）仍以 session 变量为当前会话标识。
let session = SESSION_ID;
localStorage.setItem(CKEY, session);
(function dropLegacySessionLists() {
  try {
    localStorage.removeItem("ra.sessions#" + WS_KEY);
    localStorage.removeItem("ra.sessions");
    localStorage.removeItem("ra.session");
  } catch { /* 隐私模式：忽略 */ }
})();
// 旧调用点（touchSession/autoName/renderSessions 等）保留为空操作或最简实现：
// 删掉它们要同步改 app-events.js 的发送路径，收益只是少几行——先保接口稳定，
// 语义上「单会话 = 无标题需维护、无列表需重绘」。
function touchSession() {}
function autoName() {}
function renderSessions() {}
function refreshSessionTag() {
  const tag = $("ws-tag");
  if (tag) tag.title = "当前工作区（文件工具/文件树/产物的根）";
}
// ── 车道（lane）= session（I-20261108-14 落地）────────────────────────────
// 为什么车道必须是 session 而不是「页面里的一个 tab」：内核 chat.rs 的同会话闸按
// session_id 上锁，try_lock 失败即 K409，**不排队**。所以「一个 session 里并行多个
// 任务」在语义上做不到；能并行的最小粒度就是 session。于是「一个任务一条车道」在
// 实现上等价于「一个任务一个 session_id」，窗口只是多开几个槽位同时看着它们。
//
// 车道 id 由任务 id **确定性推导**（保留给「当前所在车道」的兜底显示与将来可能
// 的按任务开会话）：host 的 /api/board 刻意不提供 task 写 op（任务只由 agent 经
// board.py 推进）。确定性推导让前后端各自算出同一个 sid：零写面、零新端点。
// 字符集与 agent-loop `trace.rs::stream_file_for` 的校验对齐（ascii 字母数字 + `-_.`）；
// `#` 是子代理会话分隔符，绝不可进车道 id。
const LANE_MAIN = SESSION_ID;
function laneOfTask(id) {
  return String(id ?? "").trim().toLowerCase()
    .replace(/[^a-z0-9._-]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 64);
}
// 车道清单 = 主车道 + 真实存在会话的 IM/任务车道。
//
// 为什么不再「每个未收口任务自动生成一条车道」（原逻辑见 git 历史 / .patchbak）：
// 车道本质是 session——切它靠 switchSession(sid) 换会话、读 <sid>.jsonl trace。
// 但 board 里的任务从来没有用 laneOfTask(id) 这个 sid 真正开过会话（trace 里没有
// t-*.jsonl），于是顶端冒出一排「T-2026xxxx-xx」标签，点进去是全空的——用户反馈
// 「任务 id 里都是空白的，没有具体任务相关的会话」。机械按状态列任务、却不保证有
// 会话内容，是假功能：它让人以为任务各自有对话，实际所有真实对话都跑在 main/qq-*。
// 修法：只把**确实开过会话**的车道列出来——主车道 + IM 会话（平台自己上报）+ 当前
// 脚下车道（兜底，见下）。任务要进入车道栏，得真的以该 sid 发生过对话，而不是靠状态。
function laneList() {
  const out = [{ sid: LANE_MAIN, label: "对话", title: "主车道：不属于任何任务的一般对话" }];
  // IMV：合并 IM 网关上报的会话（QQ/钉钉），按平台分组排在任务车道之前。
  // 元数据来自 GET /api/im/sessions（网关写 <repo>/im-gateway/data/im-sessions.json）。
  // 不写 board 账本、不改 session 切分——sid 仍是 qq-/dt- 前缀的原 session key。
  {
    const rank = { qq: 0, dingtalk: 1 };
    const labelOf = { qq: "QQ", dingtalk: "钉钉" };
    const rows = Object.entries(imSessions)
      .filter(([sid, m]) => sid && m && m.platform)
      .sort((a, b) => (rank[a[1].platform] ?? 9) - (rank[b[1].platform] ?? 9) || (b[1].last_ts || 0) - (a[1].last_ts || 0));
    for (const [sid, m] of rows) {
      if (out.some((l) => l.sid === sid)) continue;
      const tag = labelOf[m.platform] || m.platform;
      const who = String(m.chat_id || "").slice(-6) || String(m.user_id || "").slice(-6);
      const when = m.last_ts ? new Date(m.last_ts).toLocaleString() : "";
      out.push({ sid, label: `${tag}·${who}`, title: `${tag} 会话 ${sid}${when ? " · " + when : ""}` });
    }
  }
  // IMV：合并 IM 网关上报的会话（QQ/钉钉），按平台分组排在前面。
  // 元数据来自 GET /api/im/sessions（网关写 <repo>/im-gateway/data/im-sessions.json）。
  // 不写 board 账本、不改 session 切分——sid 仍是 qq-/dt- 前缀的原 session key。
  {
    const rank = { qq: 0, dingtalk: 1 };
    const labelOf = { qq: "QQ", dingtalk: "钉钉" };
    const rows = Object.entries(imSessions)
      .filter(([sid, m]) => sid && m && m.platform)
      .sort((a, b) => (rank[a[1].platform] ?? 9) - (rank[b[1].platform] ?? 9) || (b[1].last_ts || 0) - (a[1].last_ts || 0));
    for (const [sid, m] of rows) {
      if (out.some((l) => l.sid === sid)) continue;
      const tag = labelOf[m.platform] || m.platform;
      const who = String(m.chat_id || "").slice(-6) || String(m.user_id || "").slice(-6);
      const when = m.last_ts ? new Date(m.last_ts).toLocaleString() : "";
      out.push({ sid, label: `${tag}·${who}`, title: `${tag} 会话 ${sid}${when ? " · " + when : ""}` });
    }
  }
  // 兜底：当前所在车道必须出现。任务侧不再自动列出空车道，但用户脚下正站着的那条
  // （可能是一条任务 sid）必须保留——抽走它等于让人失去方向。label 用任务标题而非
  // 裸 id，既认得出是哪件事，也不会再出现「一串看不懂的编号」。
  if (!out.some((l) => l.sid === session) && session !== LANE_MAIN) {
    const bd = typeof boardData !== "undefined" ? boardData : null;
    const tasks = bd && bd.board && Array.isArray(bd.board.tasks) ? bd.board.tasks : [];
    const own = tasks.find((t) => t && t.id && laneOfTask(t.id) === session);
    out.push({ sid: session, label: own ? String(own.title || own.id) : session,
      title: own ? `${own.id} · ${own.title ?? ""}`.trim() : `车道 ${session}` });
  }
  return out;
}
function renderLanes() {
  const bar = $("lanes");
  if (!bar) return;
  const lanes = laneList();
  // 只有主车道时整条隐藏：单任务使用不该多出一排视觉噪音。
  bar.hidden = lanes.length < 2;
  bar.innerHTML = "";
  for (const l of lanes) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "lane" + (l.sid === session ? " on" : "") + (laneBusy.get(l.sid) ? " busy" : "");
    b.title = l.title + (laneBusy.get(l.sid) ? "（运行中）" : "");
    b.textContent = l.label;
    b.onclick = () => switchSession(l.sid);
    bar.appendChild(b);
  }
}
// 后台车道在跑什么，用户必须看得见——否则「切走不打断」这个能力是隐形的，用户既不
// 敢切，也不知道切走之后还在不在干活。
// 只显示「运行中」这一种事实（来源 /api/chat/status 的 status.running），不臆造「未读」：
// 未读要比对每条车道 trace 的游标，为此轮询整份 trace 不值当，切过去自然就看得到。
const laneBusy = new Map();
// IMV：IM 会话元数据缓存。网关低频追加，页面启动时拉一次 + 定时兜底刷新。
// 只存 laneList 渲染所需的最小视图；拉取失败保持上次已知值（不闪空）。
let imSessions = {};
async function imSessionsRefresh() {
  try {
    const r = await fetch("/api/im/sessions");
    const v = await r.json();
    const next = v && v.sessions && typeof v.sessions === "object" && !Array.isArray(v.sessions) ? v.sessions : {};
    if (JSON.stringify(next) !== JSON.stringify(imSessions)) {
      imSessions = next;
      renderLanes();
    }
  } catch (_) { /* 断线：保持上次已知值 */ }
}
let laneTimer = null;
function lanePollTick() {
  for (const l of laneList()) {
    if (l.sid === session) continue;      // 当前车道由 syncRunUI 直接管，不重复问
    fetch(`/api/chat/status?session=${encodeURIComponent(l.sid)}`)
      .then((r) => r.json())
      .then((v) => {
        const on = !!(v && v.status && v.status.running);
        if (laneBusy.get(l.sid) !== on) { laneBusy.set(l.sid, on); renderLanes(); }
      })
      .catch(() => { /* 断线：保持上次已知状态，不闪 */ });
  }
}
function lanePollStart() {
  if (laneTimer) return;
  laneTimer = setInterval(lanePollTick, 3000);
}
// 切换车道。FE4 把 switchSession 留成空函数后，snapshotSessionFeed 成了无人调用的
// 死代码——补上调用点，「切走再切回」这条链路才真正接通。
// 为什么切走是安全的（三条依据，都实测过）：
//   ① postChatDirect / drain 的每次 UI 写入都带 `session === sid` 卫士；
//   ② stream_* 事件按 sse_sid 路由进 per-session 缓冲，不直接写共享的 feed；
//   ③ 后台车道的 SSE 不订阅，内容在 trace 里，切回来靠 after=<cursor> 追增量。
// 所以切车道**不会打断**后台车道的回合，这正是并行能成立的前提。
function switchSession(sid) {
  if (!sid || sid === session) return;
  snapshotSessionFeed(session);   // 离开前摘下活节点 + 精确游标
  session = sid;
  localStorage.setItem(CKEY, sid);
  refreshSessionTag();
  renderLanes();
  connect(undefined, true);       // 命中缓存则瞬时显示 + 增量追赶
}
refreshSessionTag();
renderLanes();
lanePollStart();
imSessionsRefresh();
setInterval(imSessionsRefresh, 15000); // IMV：IM 会话元数据低频兜底刷新
$("toggle").onclick = () => $("sidebar").classList.toggle("collapsed");
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
// X5：scrollFeed 在历史重放期被**每个事件**调用一次，而 scroll() 每次读 scrollHeight
// 都是一次 forced reflow。实测（DOM 10 万节点）：纯 append 500 次 0.4ms，
// append 后读一次 scrollHeight 要 211.5ms —— 单次 0.42ms，×14811 事件 ≈ 6.3s 纯重排，
// 而且这段全在主线程同步执行，页面在这期间完全无响应（用户感知即「加载很慢」）。
// 合并到每帧一次：语义不变（仍跟随贴底、上滚仍暂停），只是同一帧内的多次请求只执行一次。
let feedScrollQueued = false;
function scrollFeed() {
  if (!feedPinned || feedScrollQueued) return;
  feedScrollQueued = true;
  requestAnimationFrame(() => { feedScrollQueued = false; if (feedPinned) scroll(); });
}
function setStatus(t) { statusEl.textContent = t || ""; }


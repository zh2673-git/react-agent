"use strict";
const $ = (id) => document.getElementById(id);
const feed = $("feed"), input = $("input"), sendBtn = $("send"), stopBtn = $("stop"), statusEl = $("status"), connEl = $("conn");

// ── session 管理（localStorage；条目 {id,title}，旧格式 string[] 自动迁移）──
// title 为空时侧栏显示 id；发送首条消息后按消息内容自动命名；手动重命名后不再覆盖（清空标题可恢复自动命名）。
const SKEY = "ra.sessions", CKEY = "ra.session";
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

// ── 流式渲染：思考与正文分块，结束显示用量与耗时 ──
// 旁路文件只服务「边生成边看」，不落日志；最终 assistant 事件才是持久化与刷新恢复的
// 依据。两者靠 sid 对位：同 sid 复用同一个气泡，避免同一条消息渲染两次。
let streamSid = null, streamCard = null, streamBody = null, streamStats = null;
const streamBuf = { reasoning: "", text: "" };
const doneSids = new Set(); // 已完成回合的 sid：最终 assistant 事件已渲染，后续同 sid 的重连 stream_* 帧一律忽略，避免尾部覆盖/重弹

function applyStats(node, usage, elapsedMs) {
  if (!node) return;
  const parts = [];
  if (usage && (usage.input_tokens || usage.output_tokens)) {
    const inp = usage.input_tokens || 0, out = usage.output_tokens || 0, cache = usage.cache_read_tokens || 0;
    parts.push(`输入 ${inp}`);
    parts.push(`输出 ${out}`);
    if (usage.reasoning_tokens) parts.push(`思考 ${usage.reasoning_tokens}`);
    // cache_read = 命中缓存的输入 token；命中率 = cache / (input + cache)，两者互斥不相交
    if (cache) {
      const hit = Math.round((cache / (inp + cache)) * 100);
      parts.push(`缓存 ${cache}（命中 ${hit}%）`);
    }
  }
  if (elapsedMs) {
    const sec = elapsedMs / 1000;
    parts.push(`${sec.toFixed(1)}s`);
    // tok/s 口径：整段 LLM 墙钟（含多轮 + 思考生成 + 首 token 延迟），非纯正文生成速度
    const out = usage ? (usage.output_tokens || 0) : 0;
    if (out && sec > 0) parts.push(`${(out / sec).toFixed(1)} tok/s`);
  }
  if (!parts.length) { node.hidden = true; return; }
  node.hidden = false;
  node.textContent = parts.join(" · ");
}

// 过程链框（回合级）：思考段 + 工具卡统一收纳其中，最终答案在框外——无论上游
// 帧序如何（ollama 逐帧前置 / 部分网关把 reasoning 攒到最后），呈现顺序都稳定。
// 生命周期：user 事件重置；首个 reasoning/tool_call 时创建；assistant 最终事件收起。
let processBox = null, processBody = null, processThink = null;
let roundCards = []; // 本回合每轮流产生的答案卡；最终事件到达时中间卡收进过程框（严格模式必须声明）
const subBoxes = new Map(); // R11 子代理实时框：key = sub 标签（"sub-1"），value = {box, body, think, text}

function ensureProcessBox() {
  if (processBox) return;
  processBox = el(`<details class="ev process" open><summary><span class="plabel">思考与工具</span><span class="chev">›</span></summary><div class="pbody"></div></details>`);
  processBody = processBox.querySelector(".pbody");
  // R11 内容自动滑动：内容追加/增长时贴底滚动显示最新（所有内容仍按序留存，可上滚回看）。
  // 用户上滚即暂停跟随，距底 <48px 视为回到底部自动恢复；MutationObserver 同时覆盖
  // 文本增长（characterData）与卡片追加（childList），一处挂接全量生效。
  let pinned = true;
  processBody.addEventListener("scroll", () => {
    pinned = processBody.scrollHeight - processBody.scrollTop - processBody.clientHeight < 48;
  }, { passive: true });
  new MutationObserver(() => {
    if (pinned) processBody.scrollTop = processBody.scrollHeight;
    scrollFeed(); // 框内内容增长时页面同步跟随：工具卡/思考段始终保持在视口内
  }).observe(processBody, { childList: true, subtree: true, characterData: true });
  feed.appendChild(processBox);
}

// R11 子代理框：task 工具委派的过程在过程框内的专属嵌套子框实时呈现——
// 流式帧（网关 tail 子旁路文件打 sub 标签）与镜像 trace 事件（agent-loop 透传）双源同框。
function ensureSubBox(sub, task) {
  ensureProcessBox();
  let s = subBoxes.get(sub);
  if (!s || !processBody.contains(s.box)) {
    const box = el(`<details class="subagent" open><summary><span class="statedot"></span><span class="name">🔁 子代理 · ${esc(task ? String(task).slice(0, 60) : sub)}</span><span class="chev">›</span></summary><div class="subbody"></div></details>`);
    processBody.appendChild(box);
    s = { box, body: box.querySelector(".subbody"), think: null, text: null, rbuf: "", tbuf: "", titled: !!task };
    subBoxes.set(sub, s);
  } else if (task && !s.titled) {
    s.box.querySelector(".name").textContent = `🔁 子代理 · ${String(task).slice(0, 60)}`;
    s.titled = true;
  }
  return s;
}

// 产物/来源卡收纳区：W9.1 改挂「框外」页面流——过程框内只留思考/工具的滑动窗口，
// 卡片随到达序排在过程框下方（不占框内视口、不被自动滚动钉死），答案出现后由
// settleTurnEnd 移到答案之后置底。回合切换/重放清空后旧引用不在 feed 内 → 重建。
function ensureTurnTail() {
  ensureProcessBox();
  if (!turnTail || !feed.contains(turnTail)) {
    turnTail = el(`<div class="turn-tail"></div>`);
    feed.appendChild(turnTail);
  }
  return turnTail;
}

// R2 重新生成：绑定到该卡对应的 user 轮次（渲染时的最后一轮）。回滚到该问题
// （问题本身一并删除）后自动重发原文+附件——新答案覆盖旧答案位置。
function bindRegen(card) {
  const btn = card.querySelector(".rg-btn");
  if (!btn) return;
  if (!userTurns.length) { btn.remove(); return; }
  const u = userTurns[userTurns.length - 1];
  btn.onclick = () => rollbackTo(u.idx, true, u);
}

function ensureAnswerCard() {
  if (streamCard) return;
  streamCard = el(`<div class="ev assistant streaming"><div class="card"><div class="meta"><span class="who">assistant</span><span>生成中…</span><button class="rg-btn" title="回滚到本问题并重新生成">↻ 重新生成</button></div><div class="md"></div><div class="stats" hidden></div></div></div>`);
  streamBody = streamCard.querySelector(".md");
  streamStats = streamCard.querySelector(".stats");
  feed.appendChild(streamCard);
  streamCard._anchor = processThink; // 记录本轮思考段：最终收纳时插回其后，保持框内时序（思考→中间文本→工具）
  roundCards.push(streamCard);
  bindRegen(streamCard);
}

function startStream(sid) {
  if (streamCard) endStream(null); // 上一轮异常收尾（未等到最终事件）
  // W9.4 中间轮即时收纳：新一轮流一开始，此前各轮的文本卡即刻转入过程框（标注
  // 「中间轮」）——此前要等最终答案事件才收纳，运行中途看起来像"答案被包进框里"。
  if (roundCards.length && processBody) {
    for (const c of roundCards) {
      c.querySelector(".rg-btn")?.remove();
      const lbl = c.querySelector(".meta span:nth-child(2)");
      if (lbl) lbl.textContent = "中间轮";
      const anchor = c._anchor;
      if (anchor && processBody.contains(anchor)) anchor.after(c); else processBody.appendChild(c);
    }
    roundCards = [];
  }
  streamSid = sid ?? null;
  streamBuf.reasoning = ""; streamBuf.text = "";
  processThink = null; // 每轮流新开一段思考区（多轮按序混排在过程框内）
  streamCard = streamBody = streamStats = null; // 答案卡延迟到首个 text delta（保证过程框在前）
}

// W9.2 思考段：带「💭 思考」标签——裸淡文本夹在工具卡之间易被当成"没有思考"；
// 标签+正文打包插入，processThink 始终指向正文元素（textContent 更新点不变）。
// label 可选（W9.4 重放多轮思考时标注轮次）。
function newThinkBlock(parent, label) {
  const wrap = el(`<div class="think"><div class="think-label">${label || "💭 思考"}</div><div class="think-body" style="white-space:pre-wrap"></div></div>`);
  (parent || processBody).appendChild(wrap);
  return wrap.querySelector(".think-body");
}

function pushStream(ev) {
  if (ev.kind === "reasoning") {
    ensureProcessBox();
    if (!processThink) processThink = newThinkBlock();
    streamBuf.reasoning += ev.text || "";
    processThink.textContent = streamBuf.reasoning;
    processBox.open = true; // 过程进行中保持展开
  } else {
    ensureAnswerCard();
    streamBuf.text += ev.text || "";
    streamBody.textContent = streamBuf.text; // 流式期间纯文本追加，结束再渲染 markdown
    scrollFeed(); // 正文增长页面跟随（scrollFeed 贴底判断，用户上滚不抢滚动条）
  }
}

function endStream(ev) {
  if (!streamCard) return;
  streamCard.classList.remove("streaming");
  if (streamBuf.text) streamBody.innerHTML = md(streamBuf.text);
  applyStats(streamStats, ev ? ev.usage : null, ev ? ev.elapsed_ms : null);
  // streamCard 不清：多轮流共享同一张答案卡（最终 assistant 事件覆盖内容后才随 renderAssistant 清）
}

// ── R11 子代理过程渲染：镜像 trace 事件（实时 + 重放同源）与子流式帧（仅实时）双源 ──
// 镜像事件类型 user 不透传（父 trace user 序是回滚定位真相源）；assistant 兜底定稿正文
// （无流式帧的重放场景在此直接呈现每轮答案）。
function renderSubEvent(ev) {
  const s = ensureSubBox(ev.sub);
  switch (ev.type) {
    case "tool_call": {
      const card = el(`<details class="tool" open><summary><span class="statedot"></span><span class="name">⚙ ${esc(ev.name)}</span><span class="round">round ${esc(ev.round)}</span><span class="ms"></span><span class="chev">›</span></summary><div class="body"><div class="sec"><div class="lbl">参数</div><pre class="result-pre">${esc(JSON.stringify(ev.args ?? {}, null, 2))}</pre></div>${renderFileDiff(ev)}<div class="sec result-sec" style="display:none"><div class="lbl">结果</div><div class="result-pre result-body"></div></div></div></details>`);
      tagFileCard(card, ev); // V2：子代理文件卡同样可从变更抽屉定位
      s.body.appendChild(card);
      pending.set(`sub:${ev.sub}|${ev.round}|${ev.name}`, card); // key 带 sub 前缀，与主框 pending 不串位
      break;
    }
    case "tool_result": {
      const key = `sub:${ev.sub}|${ev.round}|${ev.name}`;
      let card = pending.get(key);
      if (!card) {
        card = el(`<details class="tool"><summary><span class="statedot"></span><span class="name">⚙ ${esc(ev.name)}</span><span class="round">round ${ev.round}</span><span class="ms"></span><span class="chev">›</span></summary><div class="body"><div class="sec result-sec"><div class="lbl">结果</div><div class="result-pre result-body"></div></div></div></details>`);
        s.body.appendChild(card);
      }
      card.classList.add("done", ev.ok ? "ok" : "fail");
      card.open = false;
      const ms = card.querySelector(".ms"); if (ms) ms.textContent = `${ev.ms ?? "?"}ms`;
      const rs = card.querySelector(".result-sec"); if (rs) rs.style.display = "";
      const rb = card.querySelector(".result-body"); if (rb) rb.textContent = ev.result_truncated ?? "";
      pending.delete(key);
      break;
    }
    case "assistant": {
      if (ev.sid) doneSids.add(ev.sid); // 该子轮回执已定稿：重连重放的 stream_* 帧忽略
      if (!ev.answer && !ev.reasoning) break;
      if (!s.text) { s.text = el(`<div class="sub-text"></div>`); s.body.appendChild(s.text); }
      s.text.innerHTML = md(ev.answer || "");
      break;
    }
    case "error":
      s.body.appendChild(el(`<div class="sub-text err">⚠ ${esc(ev.message ?? "子代理出错")}</div>`));
      break;
  }
}

// 子流式帧：子代理每轮 LLM 调用的思考/正文增量（sink 每轮以 "w" 覆写旁路文件，帧序同主链）。
function subStreamStart(ev) {
  if (ev.sid && doneSids.has(ev.sid)) return;
  const s = ensureSubBox(ev.sub);
  s.think = s.text = null; s.rbuf = ""; s.tbuf = ""; // 新一轮流式区重开
}
function subStreamDelta(ev) {
  if (ev.sid && doneSids.has(ev.sid)) return;
  const s = ensureSubBox(ev.sub);
  if (ev.kind === "reasoning") {
    if (!s.think) s.think = newThinkBlock(s.body); // 子框思考段同样带标签
    s.rbuf += ev.text || "";
    s.think.textContent = s.rbuf;
  } else {
    if (!s.text) { s.text = el(`<div class="sub-text"></div>`); s.body.appendChild(s.text); }
    s.tbuf += ev.text || "";
    s.text.textContent = s.tbuf;
  }
}
function subStreamEnd(ev) {
  if (ev.sid) doneSids.add(ev.sid); // 正文定稿交由镜像 assistant 事件
}

function renderAssistant(ev, ts) {
  if (!ev.answer && !ev.reasoning) return; // 工具轮内容为空，仅 tool_call 渲染
  // 流式态判定（streamCard 或 streamSid 任一在）：本回合出现过流式帧 → 无论 sid 是否
  // 对位都按回合收敛处理。末轮可能零流帧（LLM 秒回/网关攒帧），sid 仍属上一轮——旧逻辑
  // 严格比对 sid 会掉进重放分支：中间卡残留「生成中…」、过程框不收起、答案重复成卡。
  if (streamCard || streamSid) {
    // 同回合：答案卡填最终内容；过程框收起（回合完成，用户仍可点开回看）
    ensureAnswerCard();
    streamCard.classList.remove("streaming");
    const tsSpan = streamCard.querySelector(".meta span:nth-child(2)");
    if (tsSpan) tsSpan.textContent = ts || "完成";
    streamBody.innerHTML = md(ev.answer || "");
    applyStats(streamStats, ev.usage ?? null, ev.elapsed_ms ?? null);
    if (ev.reasoning) {
      if (!processThink) processThink = newThinkBlock(); // 该轮零思考帧（网关攒帧/非流式）也要落块，否则思考被丢弃
      processThink.textContent = ev.reasoning; // 兜底：流式思考帧缺失时补全
    }
    if (ev.sid) doneSids.add(ev.sid); // 标记已完成：后续同 sid 的重连 stream_* 帧忽略
    // 多轮中间卡收进过程框（中间轮文本不是最终答案）：去掉重答按钮、标注「中间轮」，
    // 按各自锚点（本轮思考段）插回框内保持时序，只留最后一张（最终答案）在框外。
    // 单轮/纯文本对话（roundCards.length ≤ 1）不动。
    if (roundCards.length > 1 && processBody) {
      for (const c of roundCards.slice(0, -1)) {
        c.querySelector(".rg-btn")?.remove();
        const lbl = c.querySelector(".meta span:nth-child(2)");
        if (lbl) lbl.textContent = "中间轮";
        const anchor = c._anchor;
        if (anchor && processBody.contains(anchor)) anchor.after(c); else processBody.appendChild(c);
      }
    }
    roundCards = [];
    if (processBox) processBox.open = false;
    streamSid = streamCard = streamBody = streamStats = null;
    filterArtifactsByAnswer(ev.answer); // 只留答案点名的产物（提及过滤）
    settleTurnEnd(); // 产物/来源卡移到答案之后（回合末位置）
    return;
  }
  // 重放 / 无流式：过程框（思考，折叠）+ 完整答案卡。思考按轮渲染（W9.4：agent-loop
  // 最终事件携带 reasonings[{round,text}] 全量轮次；旧 trace 无此字段则退回单块）。
  if (ev.reasonings?.length || ev.reasoning) {
    ensureProcessBox();
    const rounds = ev.reasonings?.length ? ev.reasonings : [{ text: ev.reasoning }];
    for (const r of rounds) {
      if (!r?.text) continue;
      const tb = newThinkBlock(null, r.round ? `💭 思考 · round ${r.round}` : null);
      tb.textContent = r.text;
      processThink = tb;
    }
    processBox.open = false; // 重放默认折叠
  }
  const card = el(`<div class="ev assistant"><div class="card"><div class="meta"><span class="who">assistant</span><span>${ts}</span><button class="rg-btn" title="回滚到本问题并重新生成">↻ 重新生成</button></div><div class="md"></div><div class="stats" hidden></div></div></div>`);
  card.querySelector(".md").innerHTML = md(ev.answer || "");
  applyStats(card.querySelector(".stats"), ev.usage ?? null, ev.elapsed_ms ?? null);
  if (ev.sid) doneSids.add(ev.sid); // 标记已完成：后续同 sid 的重连 stream_* 帧忽略
  feed.appendChild(card);
  bindRegen(card);
  // 回合收敛：过程框兜底折叠 + 产物过滤/置底（重放时按 trace 序即达此序，此为
  // 流式无帧路径的兜底；创建时本就折叠，重复设置无害）
  if (processBox) processBox.open = false;
  filterArtifactsByAnswer(ev.answer);
  settleTurnEnd();
}

// ── V1 工具卡内联 diff：edit_file 红绿行级 / write_file 全绿新增 / bash 命令回显。
// 数据全部来自 tool_call 事件的 args（已验证全量传输），零后端改动；>500 行折叠为
// 统计防大文件渲染卡顿。返回 HTML 片段（无变更语义的工具返回空串）。
function renderFileDiff(ev) {
  const a = ev.args ?? {};
  if (ev.name === "edit_file" && typeof a.old_string === "string" && typeof a.new_string === "string") {
    const oldL = a.old_string.split("\n"), newL = a.new_string.split("\n");
    const head = `<div class="lbl">变更 · ${esc(a.path ?? "")}</div>`;
    if (oldL.length + newL.length > 500) {
      return `<div class="sec">${head}<pre class="diff-stats">~${oldL.length} 行替换为 ~${newL.length} 行（内容过长，diff 已折叠）</pre></div>`;
    }
    const rows = [
      ...oldL.map((l) => `<div class="dl del">- ${esc(l)}</div>`),
      ...newL.map((l) => `<div class="dl add">+ ${esc(l)}</div>`),
    ].join("");
    return `<div class="sec">${head}<pre class="diff">${rows}</pre></div>`;
  }
  if (ev.name === "write_file" && typeof a.content === "string") {
    const lines = a.content.split("\n");
    const head = `<div class="lbl">变更 · ${esc(a.path ?? "")}</div>`;
    if (lines.length > 500) {
      return `<div class="sec">${head}<pre class="diff-stats">+${lines.length} 行（内容过长，已折叠）</pre></div>`;
    }
    return `<div class="sec">${head}<pre class="diff">${lines.map((l) => `<div class="dl add">+ ${esc(l)}</div>`).join("")}</pre></div>`;
  }
  if (ev.name === "bash" && typeof a.command === "string" && a.command) {
    return `<div class="sec"><div class="lbl">命令</div><pre class="cmd-echo">${esc(a.command)}</pre></div>`;
  }
  return "";
}
// 给文件工具卡打变更定位标记（变更抽屉「点击定位工具卡」的匹配依据）+ 暂存 args
// （无快照时 diff 视图用 old/new 反向重构变更前内容）。
function tagFileCard(card, ev) {
  if (ev.name === "write_file" || ev.name === "edit_file") {
    card._fcpath = String(ev.args?.path ?? "").replace(/\\/g, "/").toLowerCase();
    card._fcround = ev.round;
    card._fcsub = ev.sub ?? "";
    card._fcargs = ev.args ?? null;
  }
}

// ── 文件变更（R15 重构）：file_change 事件按回合聚合为答案下方「📝 文件变更 (N)」
// chip（与 🔗 来源同款收纳），点击滑出该轮抽屉；条目点击定位过程框工具卡，⇄ diff
// 打开双侧对比（快照 → args 重构 → 单侧 降级链）。子代理镜像事件同样汇入。fcSeen
// 兼做回滚撤销的区间索引（记录 turn）。
const fcSeen = new Map(); // 事件唯一键 → {path, op, round, sub, ts, undo, turn}
let turnFcBox = null;
function renderFileChange(ev) {
  const p = String(ev.path ?? "").replace(/\\/g, "/");
  if (!p) return;
  const key = `${p}|${ev.op ?? ""}|${ev.round ?? ""}|${ev.sub ?? ""}|${ev.ts ?? 0}`;
  if (fcSeen.has(key)) return;
  const it = { path: p, op: ev.op ?? "write", round: ev.round ?? "", sub: ev.sub ?? "", ts: ev.ts ?? 0, undo: ev.undo ?? null, turn: Math.max(0, userTurns.length - 1) };
  fcSeen.set(key, it);
  wsTreeCache = null; // 文件有变更 → 文件树缓存失效（下次打开重新拉取）
  const chip = turnFcBox ?? el(`<div class="srcchip" title="点击查看该轮文件变更">📝 文件变更 <span class="cnt">0</span></div>`);
  chip._items = chip._items ?? [];
  chip._items.push(it);
  chip.querySelector(".cnt").textContent = chip._items.length;
  chip.onclick = () => openFcDrawer(chip._items);
  if (!turnFcBox) { ensureTurnTail().appendChild(chip); turnFcBox = chip; scrollFeed(); }
}
function resetFileChanges() { fcSeen.clear(); }
function openFcDrawer(items) {
  let d = $("fc-drawer");
  if (!d) {
    d = el(`<div id="fc-drawer" class="sd-overlay"><div class="sd-panel"><div class="sd-head"><span>📝 文件变更</span><button class="btn ghost sd-close">关闭</button></div><div class="sd-body"></div></div></div>`);
    document.body.appendChild(d);
    d.onclick = (e) => { if (e.target === d) d.classList.remove("open"); };
    d.querySelector(".sd-close").onclick = () => d.classList.remove("open");
  }
  const body = d.querySelector(".sd-body");
  body.innerHTML = "";
  if (!items?.length) body.innerHTML = '<div class="fc-empty">该轮尚无文件变更（write_file / edit_file 成功及 bash 快照区改动时记录）</div>';
  for (const it of items ?? []) {
    const row = el(`<div class="sd-item fc-item" title="点击定位对应工具卡"><span class="fc-op ${it.op === "edit" ? "edit" : "write"}">${{ edit: "编辑", bash: "脚本", write: "写入" }[it.op] ?? it.op}</span><span class="fc-path">${esc(it.path)}</span><span class="u">${it.sub ? `子代理 ${esc(it.sub)}` : `round ${esc(it.round)}`}</span><button class="btn ghost fc-diff" title="对比变更前后内容">⇄ diff</button></div>`);
    row.onclick = () => { d.classList.remove("open"); locateFileChange(it); };
    row.querySelector(".fc-diff").onclick = (e) => { e.stopPropagation(); d.classList.remove("open"); openFcDiff(it); };
    body.appendChild(row);
  }
  d.classList.add("open");
}
function locateFileChange(it) {
  const cards = [...document.querySelectorAll("#feed details.tool")].filter(
    (c) => c._fcpath === it.path.toLowerCase() && String(c._fcround) === String(it.round) && !it.sub === !c._fcsub
  );
  const card = cards[cards.length - 1]; // 同轮同路径多次变更 → 定位最近一张
  if (!card) return;
  card.open = true;
  card.scrollIntoView({ behavior: "smooth", block: "center" });
  card.classList.add("fc-flash");
  setTimeout(() => card.classList.remove("fc-flash"), 1600);
}

// ── W14 工作区文件树：头部「📁」→ 左侧滑出面板。数据源 GET /api/tree（扁平清单，
// 剪枝噪音/点目录），前端建树；点击文件复用产物卡同一条预览链（文本→Monaco/md 富
// 渲染；html/图片/pdf→新标签原生渲染；其余→/files?download=1 下载）。
let wsTreeCache = null;
function toggleWsTree() {
  let d = $("ws-drawer");
  if (!d) {
    d = el(`<div id="ws-drawer" class="wsp-overlay"><div class="wsp-panel"><div class="wsp-head"><span>📁 工作区文件</span><span class="wsp-cnt"></span><button class="btn ghost wsp-close">关闭</button></div><div class="wsp-body"></div></div></div>`);
    document.body.appendChild(d);
    d.onclick = (e) => { if (e.target === d) d.classList.remove("open"); };
    d.querySelector(".wsp-close").onclick = () => d.classList.remove("open");
  }
  const opening = !d.classList.contains("open");
  d.classList.toggle("open", opening);
  if (opening) loadWsTree(d.querySelector(".wsp-body"), d.querySelector(".wsp-cnt"));
}
async function loadWsTree(body, cnt) {
  body.innerHTML = '<div class="wsp-empty">加载中…</div>';
  try {
    if (!wsTreeCache) {
      const r = await fetch("/api/tree");
      if (!r.ok) throw new Error("HTTP " + r.status);
      wsTreeCache = await r.json();
    }
    cnt.textContent = `${wsTreeCache.count} 个文件` + (wsTreeCache.truncated ? "（已截断）" : "");
    body.innerHTML = "";
    body.appendChild(buildWsTree(wsTreeCache.files.map((f) => f.path)));
  } catch (e) { body.innerHTML = `<div class="wsp-empty">加载失败: ${esc(e.message)}</div>`; }
}
function buildWsTree(paths) {
  const root = { dirs: new Map(), files: [] };
  for (const p of paths) {
    const segs = p.split("/");
    let node = root;
    for (let i = 0; i < segs.length - 1; i++) {
      if (!node.dirs.has(segs[i])) node.dirs.set(segs[i], { dirs: new Map(), files: [] });
      node = node.dirs.get(segs[i]);
    }
    node.files.push(segs[segs.length - 1]);
  }
  const build = (node, prefix) => {
    const box = el(`<div class="wsp-dir"></div>`);
    for (const [name, child] of [...node.dirs.entries()].sort((a, b) => a[0].localeCompare(b[0]))) {
      const det = el(`<details open><summary>📁 ${esc(name)}</summary></details>`);
      det.appendChild(build(child, prefix + name + "/"));
      box.appendChild(det);
    }
    for (const f of node.files.sort((a, b) => a.localeCompare(b))) {
      const full = prefix + f;
      const row = el(`<div class="wsp-file" title="${esc(full)}">📄 ${esc(f)}</div>`);
      row.onclick = () => openWsFile(full);
      box.appendChild(row);
    }
    return box;
  };
  return build(root, "");
}
function openWsFile(path) {
  const ext = (path.includes(".") ? path.split(".").pop() : "").toLowerCase();
  if (PREVIEW_TEXT_EXTS.includes(ext)) previewTextFile(path, ext);
  else if (PREVIEW_NATIVE_EXTS.includes(ext)) window.open(filesUrl(path), "_blank");
  else window.open(filesUrl(path, true), "_blank");
}

// ── 事件渲染 ──
const pending = new Map();
// R2：本回合内已渲染的 user 轮次（idx = trace 内第几条 user 事件，0 基）。
// 回滚按「第 N 条 user 消息」定位；重新生成复用最后一轮的原文+附件（trace user 事件带全量 b64）。
let userTurns = [];
// 回合级卡片追踪：产物卡/来源卡流式期间按到达序即时渲染（反馈优先），回合收敛时
// 统一移到最终答案之后置底（appendChild 对既有节点即移动）。来源卡回合内累计去重。
let turnArtifacts = [];
let turnSourceBox = null;
let turnTail = null; // 框外收纳区：流式期间的产物/来源卡随到达序排过程框下方（W9.1），答案出现后移出置底
const turnSourceUrls = new Set();
const turnArtifactPaths = new Set(); // 同回合同路径只出一张卡（write_file 结构化 + 答案兜底双登记）
function render(ev) {
  const ts = ev.ts ? new Date(ev.ts).toLocaleTimeString() : "";
  // R11 子代理事件路由：带 sub 标签的事件（镜像 trace + 子流式帧）进子框，不与主链
  // 渲染（气泡/答案卡/主 pending 表）串线；artifact/sources/file_change 除外——子代理
  // 生成的成品文件、引用来源与文件变更同样汇入主链卡片收纳区。
  if (ev.sub && ev.type !== "artifact" && ev.type !== "sources" && ev.type !== "file_change") {
    const t = String(ev.type);
    if (t === "stream_start") subStreamStart(ev);
    else if (t === "stream_delta") subStreamDelta(ev);
    else if (t === "stream_end") subStreamEnd(ev);
    else if (t === "stream_error") { /* 子轮异常：镜像 error 事件会跟进 */ }
    else renderSubEvent(ev);
    scrollFeed();
    return;
  }
  switch (ev.type) {
    case "user": {
      doneSids.clear(); // 新回合开始：作废上一回合标记，防止同 round sid 碰撞误伤新回合流式
      processBox = processBody = processThink = null; // 过程链框按用户回合划分
      subBoxes.clear(); // 子代理框按用户回合划分（引用随 DOM 重建，旧引用弃用）
      roundCards = []; // 答案卡按用户回合累积，新回合清空
      turnArtifacts = []; turnSourceBox = null; turnSourceUrls.clear(); turnArtifactPaths.clear(); turnTail = null; turnFcBox = null; // 回合级卡片追踪归位（卡已在 DOM，仅弃引用）
      const idx = userTurns.length;
      userTurns.push({ idx, text: ev.text ?? "", atts: ev.attachments ?? [] });
      // R3 附件重放：图片缩略图 / 文件条（trace user 事件带全量 b64）
      const attsHtml = (ev.attachments ?? []).map((a) =>
        a.mime && a.mime.startsWith("image/") && a.data_b64
          ? `<img class="batt-img" alt="${esc(a.name)}" src="data:${esc(a.mime)};base64,${a.data_b64}">`
          : `<span class="batt-file">📄 ${esc(a.name)}</span>`
      ).join("");
      const uel = el(`<div class="ev user"><button class="rb-btn" title="删除该问题及其后的对话，回到提问前">⤺ 回滚</button><div class="bubble">${attsHtml ? `<div class="batts">${attsHtml}</div>` : ""}<div class="txt">${esc(ev.text)}</div></div></div>`);
      uel.querySelector(".rb-btn").onclick = () => rollbackTo(idx, false);
      feed.appendChild(uel);
      break;
    }
    case "assistant":
      renderAssistant(ev, ts);
      break;
    case "artifact":
      renderArtifactCard(ev);
      break;
    case "sources":
      renderSourcesCard(ev); // 信息溯源：web 检索/阅读引用链接
      break;
    case "file_change":
      renderFileChange(ev); // R15：按回合聚合 chip + 抽屉（定位 / diff），兼回滚撤销索引
      break;
    case "stream_start":
      if (!(ev.sid && doneSids.has(ev.sid))) startStream(ev.sid);
      break;
    case "stream_delta":
      if (!(ev.sid && doneSids.has(ev.sid))) pushStream(ev);
      break;
    case "stream_end":
      if (!(ev.sid && doneSids.has(ev.sid))) endStream(ev);
      break;
    case "stream_error":
      if (!(ev.sid && doneSids.has(ev.sid))) endStream(null);
      break;
    case "tool_call": {
      ensureProcessBox(); // 工具卡与思考段同框（过程链）
      const card = el(`<details class="tool" open><summary><span class="statedot"></span><span class="name">⚙ ${esc(ev.name)}</span><span class="round">round ${esc(ev.round)}</span><span class="ms"></span><span class="chev">›</span></summary><div class="body"><div class="sec"><div class="lbl">参数</div><pre class="result-pre">${esc(JSON.stringify(ev.args ?? {}, null, 2))}</pre></div>${renderFileDiff(ev)}<div class="sec result-sec" style="display:none"><div class="lbl">结果</div><div class="result-pre result-body"></div></div></div></details>`);
      tagFileCard(card, ev); // V2：文件工具卡打定位标记（抽屉点击回跳）
      processBody.appendChild(card);
      pending.set(`${ev.round}|${ev.name}`, card);
      break;
    }
    case "tool_result": {
      const key = `${ev.round}|${ev.name}`;
      let card = pending.get(key);
      if (!card) {
        ensureProcessBox();
        card = el(`<details class="tool"><summary><span class="statedot"></span><span class="name">⚙ ${esc(ev.name)}</span><span class="round">round ${ev.round}</span><span class="ms"></span><span class="chev">›</span></summary><div class="body"><div class="sec result-sec"><div class="lbl">结果</div><div class="result-pre result-body"></div></div></div></details>`);
        processBody.appendChild(card);
      }
      card.classList.add("done", ev.ok ? "ok" : "fail");
      card.open = false; // 执行结束自动收起（用户仍可点开看参数与结果）
      const ms = card.querySelector(".ms"); if (ms) ms.textContent = `${ev.ms ?? "?"}ms`;
      const rs = card.querySelector(".result-sec"); if (rs) rs.style.display = "";
      const rb = card.querySelector(".result-body"); if (rb) rb.textContent = ev.result_truncated ?? "";
      pending.delete(key);
      break;
    }
    case "subagent": {
      // R11：委派标记 → 在过程框内创建「子代理」实时子框（task 作标题；后续镜像
      // 事件与子流式帧按 sub 标签汇入同框）。不再是一条静态卡片。
      const sub = String(ev.sub_session ?? "").split("#").pop() || "sub";
      ensureSubBox(sub, ev.task ?? "");
      break;
    }
    case "compaction":
      feed.appendChild(el(`<div class="ev compaction"><span class="line">── 上下文已压缩（摘要 ${esc(ev.summarized)} 条，保留 ${esc(ev.kept)} 条）──</span></div>`));
      break;
    case "skill_installed": {
      // R9/W6：技能就绪内联卡（过程框内，样式同工具卡族）。pending 空 → 仅「已注册」无按钮。
      // 启用永远走配置闸：点击时拉取当前 config 求「现有 ∪ pending」并集（重放卡同样安全）。
      ensureProcessBox();
      const loaded = ev.tools_loaded ?? [];
      const pend = ev.tools_pending ?? [];
      const card = el(`<details class="tool skill-installed" open><summary><span class="statedot"></span><span class="name">🧩 技能 ${esc(ev.skill)} 已就绪</span><span class="round">${pend.length ? `工具 ${pend.length} 件待启用` : "已注册"}</span><span class="chev">›</span></summary><div class="body"><div class="sec"><div class="lbl">配套工具（装载 ≠ 启用）</div><div class="si-tools"></div><div class="si-actions"></div></div></div></details>`);
      const toolsBox = card.querySelector(".si-tools");
      if (!(loaded.length || pend.length)) {
        toolsBox.innerHTML = '<span class="si-note">无配套工具（仅注册，技能正文下轮对话可见）</span>';
      } else {
        for (const t of loaded) {
          const isPending = pend.includes(t);
          toolsBox.appendChild(el(`<div class="si-tool"><span class="si-dot ${isPending ? "pending" : "on"}"></span>${esc(t)}${isPending ? ' <span class="si-tag">待启用</span>' : ""}</div>`));
        }
      }
      const actions = card.querySelector(".si-actions");
      if (pend.length) {
        const btn = el(`<button class="btn ghost si-enable">一键启用（${pend.length}）</button>`);
        btn.onclick = async () => {
          btn.disabled = true;
          try {
            const c = (await api("GET", "/api/config")).config;
            const cur = new Set((c.tools ?? []).filter((x) => x.enabled).map((x) => x.name));
            for (const t of pend) cur.add(t);
            await api("PUT", "/api/config", { tools: { enabled: [...cur] } });
            actions.innerHTML = '<span class="si-note">已启用 · 在会话中 load_skill 后生效</span>';
            card.querySelector(".statedot").style.background = "var(--ok)";
            loadConfig(); // 刷新 cfgTools 缓存（若抽屉已开）
          } catch (e) {
            btn.disabled = false;
            actions.innerHTML = `<span class="si-note">启用失败: ${esc(e.message)}</span>`;
          }
        };
        actions.appendChild(btn);
      }
      processBody.appendChild(card);
      break;
    }
    case "error": {
      const em = ev.message;
      const emsg = typeof em === "string" ? em : (em && em.message) ? em.message : (em ? JSON.stringify(em) : "");
      feed.appendChild(el(`<div class="ev error"><div class="banner">✗ [${esc(ev.where ?? "?")}] ${esc(emsg)}</div></div>`));
      setStatus("出错");
      break;
    }
      break;
    default:
      return; // 未知事件类型：忽略（前向兼容）
  }
  scrollFeed();
}

// ── SSE：增量续传。after=0=全量重放（首屏/切换会话，清空 feed）；after>0=断线续传（保留历史，只补新增）。
// host 在重放阶段只推持久 trace 事件、不推流式旁路（最终 assistant 已含完整内容），catch up 后才推 stream_*。
// 这样刷新/重连都只补增量：既不叠加重复气泡，也不整段重绘。
let es = null, lastAfter = 0;
function connect(after) {
  after = after || 0;
  if (es) es.close();
  if (after === 0) { feed.innerHTML = ""; pending.clear(); lastAfter = 0; doneSids.clear(); processBox = processBody = processThink = null; subBoxes.clear(); userTurns = []; turnArtifacts = []; turnSourceBox = null; turnSourceUrls.clear(); turnArtifactPaths.clear(); turnTail = null; turnFcBox = null; resetFileChanges(); } // 首屏/切换/回滚后：清空后全量重放
  // 清 DOM 后流式句柄一并失效，避免继续写入已移除的气泡
  streamSid = streamCard = streamBody = streamStats = null;
  es = new EventSource(`/api/events?session=${encodeURIComponent(session)}&after=${after}`);
  es.onopen = () => { connEl.classList.add("live"); setStatus(""); };
  es.onerror = () => { connEl.classList.remove("live"); setStatus("连接断开，重连中…"); connect(lastAfter); }; // 增量续传，不清空
  es.onmessage = (m) => {
    let ev; try { ev = JSON.parse(m.data); } catch { return; }
    if (ev && ev.type && !String(ev.type).startsWith("stream_")) lastAfter++; // 持久事件数 ≈ trace 游标
    try { render(ev); } catch (e) { console.warn("render failed:", ev?.type, e); } // 单事件渲染异常不拖垮游标与后续事件
  };
}
connect();

// ── R3 附件：待发托盘（FileReader → base64；与 host 校验口径一致：≤4 个、单个 ≤2MB）──
const ATT_MAX_COUNT = 4, ATT_MAX_BYTES = 2000000;
let pendingAtts = [];
const EXT_MIME = { md: "text/plain", markdown: "text/plain", txt: "text/plain", log: "text/plain",
  py: "text/x-python", js: "text/javascript", ts: "text/typescript", tsx: "text/typescript",
  rs: "text/x-rust", go: "text/x-go", java: "text/x-java", c: "text/x-c", cpp: "text/x-c", h: "text/x-c",
  json: "application/json", toml: "text/plain", yaml: "text/plain", yml: "text/plain",
  xml: "text/xml", html: "text/html", css: "text/css", sql: "text/plain", sh: "text/x-sh", csv: "text/csv" };

$("attach").onclick = () => $("file").click();
$("file").onchange = async (e) => { for (const f of e.target.files) await addAttach(f); e.target.value = ""; };
input.addEventListener("paste", (e) => {
  const files = [...(e.clipboardData?.files ?? [])];
  if (files.length) { e.preventDefault(); files.forEach(addAttach); }
});

function addAttach(f) {
  return new Promise((done) => {
    if (pendingAtts.length >= ATT_MAX_COUNT) { setStatus(`附件最多 ${ATT_MAX_COUNT} 个`); return done(); }
    if (f.size > ATT_MAX_BYTES) { setStatus(`附件 ${f.name} 超过 2MB 上限`); return done(); }
    const mime = f.type || EXT_MIME[String(f.name.split(".").pop() || "").toLowerCase()] || "application/octet-stream";
    const r = new FileReader();
    r.onload = () => {
      pendingAtts.push({ name: f.name, mime, data_b64: String(r.result).split(",")[1] || "" });
      renderAtts(); done();
    };
    r.onerror = () => { setStatus(`读取 ${f.name} 失败`); done(); };
    r.readAsDataURL(f);
  });
}
function renderAtts() {
  const box = $("atts");
  box.innerHTML = ""; box.hidden = !pendingAtts.length;
  pendingAtts.forEach((a, i) => {
    const item = el(`<div class="att">${a.mime.startsWith("image/") ? `<img alt="" src="data:${esc(a.mime)};base64,${a.data_b64}">` : ""}<span>${esc(a.name)}</span><span class="rm" title="移除">×</span></div>`);
    item.querySelector(".rm").onclick = () => { pendingAtts.splice(i, 1); renderAtts(); };
    box.appendChild(item);
  });
}

// chatAbort：当次 /api/chat 的中断器。停止 = 先 POST cancel（置位服务端取消信号），
// 再 abort 阻塞中的请求——UI 即时解锁；服务端到最近检查点（波次间/轮次边界）自然收敛 K499，
// memory/trace 照常落盘，重连 SSE 即见真实状态。
let chatAbort = null;
async function send(presetText, presetAtts) {
  const text = (presetText ?? input.value).trim();
  if (!text) return;
  // 附件：手动发送取托盘待发件；重新生成用 trace 留存的原始附件
  const atts = presetAtts ? presetAtts : pendingAtts.splice(0);
  if (presetText === undefined) { input.value = ""; input.style.height = "48px"; renderAtts(); }
  sendBtn.disabled = true; stopBtn.hidden = false; setStatus("思考中…");
  touchSession(session);
  autoName(session, text); // 标题为空时按首条消息自动命名
  try {
    chatAbort = new AbortController();
    const r = await fetch("/api/chat", {
      method: "POST", headers: { "content-type": "application/json" }, signal: chatAbort.signal,
      body: JSON.stringify({ session_id: session, message: text, ...(atts.length ? { attachments: atts } : {}) }),
    });
    const v = await r.json();
    if (!v.ok) setStatus("失败: " + (v.error?.message ?? r.status));
    else setStatus("");
  } catch (e) {
    if (e?.name === "AbortError") setStatus("已停止（后台正在收敛）"); else setStatus("请求失败: " + e);
  }
  finally { chatAbort = null; sendBtn.disabled = false; stopBtn.hidden = true; input.focus(); }
}
sendBtn.onclick = () => send();

// R2 回滚：物理截断到第 idx 条 user 消息之前（该问题及其后全部删除，memory 消息与
// trace 事件同源截断）。R15：区间内 agent 的文件改动随回滚一并撤销（host 冲突检测：
// 变更后又被人工改过的文件自动跳过，绝不硬覆盖）。resend=true 为「重新生成」。
async function rollbackTo(idx, resend, ut) {
  if (sendBtn.disabled) { setStatus("对话进行中，请先停止或等待完成"); return; }
  const affected = [...fcSeen.values()].filter((e) => e.turn >= idx);
  const fileList = affected.slice(0, 8).map((e) => `${{ edit: "编辑", bash: "脚本", write: "写入" }[e.op] ?? e.op} ${e.path}`).join("\n");
  const undoNote = affected.length
    ? `\n\n同时撤销该轮起 agent 的文件改动（${affected.length} 个）：\n${fileList}${affected.length > 8 ? `\n…等共 ${affected.length} 个` : ""}\n（变更后又被手动修改的文件将自动跳过）`
    : "";
  if (!confirm(resend ? `回滚本问题并重新生成回答？${undoNote}` : `回滚到该问题之前？（该问题及其后的对话将被删除）${undoNote}`)) return;
  try {
    const v = await api("POST", "/api/chat/rollback", { session_id: session, upto_user_index: idx });
    const un = v.undone?.length ?? 0, sk = v.skipped?.length ?? 0;
    var undoMsg = (un || sk) ? `已撤销 ${un} 个文件改动` + (sk ? `，${sk} 个跳过（文件已被修改）` : "") : null;
  } catch (e) { setStatus("回滚失败: " + e.message); return; }
  connect(0); // 全量重放截断后的历史（重置 SSE 游标）
  if (undoMsg) setStatus(undoMsg);
  if (resend && ut) send(ut.text, ut.atts || []);
}

// P2/T1 停止：发送期间可见。置位取消信号（agent-loop 在波次间/轮次边界收敛为 K499），
// 随即 abort 阻塞中的 /api/chat（UI 即时解锁，不再等服务端收敛）。
// 与发送互斥——stopBtn 仅在 sendBtn.disabled 时出现。
stopBtn.onclick = async () => {
  stopBtn.disabled = true;
  try {
    const r = await fetch(`/api/chat/cancel?session=${encodeURIComponent(session)}`, { method: "POST" });
    const v = await r.json();
    if (v.ok) {
      setStatus("正在停止…");
      chatAbort?.abort();
    } else setStatus("停止失败: " + (v.error?.message ?? r.status));
  } catch (e) { setStatus("停止失败: " + e); }
  finally { stopBtn.disabled = false; }
};
input.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); send(); }
});
input.addEventListener("input", () => {
  input.style.height = "48px";
  input.style.height = Math.min(input.scrollHeight, 180) + "px";
});
input.focus();

// ── 设置面板（LLM / 工具白名单 / 技能 CRUD）──
const drawer = $("drawer");
$("gear").onclick = () => { drawer.classList.add("open"); loadConfig(); loadSkills(); };
$("dclose").onclick = () => drawer.classList.remove("open");
document.querySelectorAll("#drawer .tabs button").forEach((b) => b.onclick = () => {
  document.querySelectorAll("#drawer .tabs button").forEach((x) => x.classList.toggle("on", x === b));
  for (const t of ["llm", "tools", "skills", "agent"]) $("tab-" + t).hidden = t !== b.dataset.tab;
});

function flash(id, ok, msg) {
  const e = $(id);
  e.textContent = msg; e.className = "flash " + (ok ? "ok" : "err");
  setTimeout(() => { if (e.textContent === msg) e.textContent = ""; }, 4000);
}
async function api(method, url, body) {
  const r = await fetch(url, {
    method,
    headers: body ? { "content-type": "application/json" } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  const v = await r.json().catch(() => ({}));
  if (!r.ok || v.ok === false) throw new Error(v.error?.message ?? r.status);
  return v;
}

let cfgTools = [], cfgMcp = { servers: [], tools: [] };
async function loadConfig() {
  try {
    await loadPresets(); // 站点清单就绪后再回显，base_url 反查预设
    const c = (await api("GET", "/api/config")).config;
    $("llm-provider").value = c.llm.provider;
    const model = c.llm.model ?? "";
    const sel = $("llm-model");
    if ([...sel.options].some((o) => o.value === model)) {
      sel.value = model; $("llm-model-custom").value = "";
    } else {
      sel.value = "__custom__"; $("llm-model-custom").value = model;
    }
    syncModelField();
    const isOllama = c.llm.provider === "ollama";
    $("llm-base").value = isOllama ? (c.llm.ollama_host ?? "localhost:11434") : (c.llm.base_url ?? "");
    $("llm-key").value = "";
    $("llm-key").placeholder = c.llm.key?.key_set ? `已设置（尾号 ${c.llm.key.key_tail}，留空不修改）` : "未设置";
    if (sitePresets.length && c.llm.provider === "openai") { $("llm-site").value = curSiteId(); updateSiteLink(curSiteId()); }
    cfgTools = c.tools ?? [];
    cfgMcp = c.mcp ?? { servers: [], tools: [] };
    if (!$("tab-tools").hidden) renderTools(); // 工具 tab 可见时回填（覆盖 fetch 完成晚于 tab 切换的时序）
    // （MCP key 配置已移入「MCP 外接」各服务组内，随 renderTools 渲染）
    syncLlmFields();
    const a = c.agent ?? {};
    for (const [id, v] of [
      ["agent-max-rounds", a.max_rounds], ["agent-output-dir", a.output_dir ?? "outputs"], ["agent-budget-secs", a.budget_secs], ["agent-token-budget", a.token_budget],
      ["agent-history-limit", a.history_limit], ["agent-compact-trigger", a.compact_trigger], ["agent-compact-keep", a.compact_keep],
      ["agent-tool-result-limit", a.tool_result_limit], ["agent-retry-attempts", a.retry_attempts], ["agent-retry-base-ms", a.retry_base_ms],
      ["agent-llm-context-tokens", a.llm_context_tokens],
    ]) $(id).value = v ?? "";
    $("agent-system-prompt").value = a.system_prompt ?? "";
  } catch (e) { flash("llm-flash", false, "加载失败: " + e.message); }
}
$("llm-fetch-models").onclick = () => fetchModels(false);
// models_meta：/api/models 可选扩展（ollama 原生窗口探测），其他 provider 为空数组——零硬编码
let modelsMeta = [];
function fmtCtx(n) { return n >= 1024 ? Math.round(n / 1024) + "k" : String(n); }
function curModelName() {
  const msel = $("llm-model");
  return msel.value === "__custom__" ? $("llm-model-custom").value.trim() : msel.value;
}
// Agent 页 llm_context_tokens 提示 + 「填入原生值」（L7：按 provider 联动——仅本地窗口型生效）
function updateCtxHint() {
  const hit = modelsMeta.find((m) => m.name === curModelName() && m.ctx_limit);
  const hint = $("ctx-hint"), fill = $("ctx-fill");
  const p = $("llm-provider").value;
  if (p !== "ollama") {
    // 云端 API：窗口由服务端管理，token 闸与 num_ctx 均不生效（值保留，切回本地部署自动恢复）
    hint.textContent = p === "openai" || p === "anthropic"
      ? `当前 provider（${p}）为云端 API：窗口由服务端管理，此设置不生效（切回本地部署时自动恢复作用）`
      : "此设置仅本地窗口型 provider（ollama 等）生效";
    fill.hidden = true;
    return;
  }
  if (hit) {
    hint.textContent = `当前模型原生窗口 ${fmtCtx(hit.ctx_limit)} tokens（为速度可调小；原生值仅参考，非必填）`;
    fill.hidden = false;
  } else if (modelsMeta.length) {
    hint.textContent = "未探测到该模型的原生窗口（自定义模型或响应缺失）；为速度可调小";
    fill.hidden = true;
  } else {
    hint.textContent = "原生窗口：未拉取模型列表（LLM 页「拉取模型」后可用）；为速度可调小";
    fill.hidden = true;
  }
}
$("ctx-fill").onclick = () => {
  const hit = modelsMeta.find((m) => m.name === curModelName() && m.ctx_limit);
  if (hit) $("agent-llm-context-tokens").value = hit.ctx_limit;
};
async function fetchModels(silent) {
  const btn = $("llm-fetch-models");
  const prev = btn.textContent;
  if (!silent) { btn.disabled = true; btn.textContent = "拉取中…"; }
  try {
    const v = await api("GET", "/api/models?provider=" + encodeURIComponent($("llm-provider").value));
    if (v && v.ok && Array.isArray(v.models) && v.models.length) {
      const sel = $("llm-model");
      const cur = sel.value && sel.value !== "__custom__" ? sel.value : "";
      modelsMeta = Array.isArray(v.models_meta) ? v.models_meta : [];
      const ctxByName = new Map(modelsMeta.filter((m) => m.ctx_limit).map((m) => [m.name, m.ctx_limit]));
      sel.innerHTML = "";
      for (const m of v.models) {
        const o = document.createElement("option");
        o.value = m;
        o.textContent = ctxByName.has(m) ? `${m} · ${fmtCtx(ctxByName.get(m))}` : m;
        sel.appendChild(o);
      }
      const oc = document.createElement("option");
      oc.value = "__custom__"; oc.textContent = "（自定义…）"; sel.appendChild(oc);
      sel.value = (cur && v.models.includes(cur)) ? cur : (v.models[0] || "__custom__");
      syncModelField();
      if (!silent) flash("llm-flash", true, `已拉取 ${v.models.length} 个模型，点 model 下拉选择`);
    } else if (!silent) {
      flash("llm-flash", false, "未返回模型列表：" + (v?.error?.message || "空"));
    }
  } catch (e) {
    if (!silent) flash("llm-flash", false, "拉取失败: " + e.message);
  } finally {
    if (!silent) { btn.disabled = false; btn.textContent = prev; }
    updateCtxHint();
  }
}
// 打开设置时静默拉一次（仅 ollama）：Agent 页原生窗口提示开箱可用，失败不打扰
async function ensureModelsMeta() {
  if (modelsMeta.length || $("llm-provider").value !== "ollama") return;
  await fetchModels(true);
}
function syncModelField() {
  const isCustom = $("llm-model").value === "__custom__";
  $("llm-model").style.display = isCustom ? "none" : "";
  const ci = $("llm-model-custom");
  ci.style.display = isCustom ? "" : "none";
  if (isCustom && !ci.value) ci.focus();
}
$("llm-model").addEventListener("change", () => { syncModelField(); updateCtxHint(); });
$("llm-model-custom").addEventListener("input", updateCtxHint);
$("llm-provider").addEventListener("change", syncLlmFields);
// L6 站点预设：一键切换 OpenAI 兼容站点（数据源 /api/presets → presets.py；保存 = configure 热应用，零重启）
let sitePresets = [];
async function loadPresets() {
  if (sitePresets.length) return;
  try {
    const v = await api("GET", "/api/presets");
    if (v && v.ok && Array.isArray(v.presets) && v.presets.length) {
      sitePresets = v.presets;
      const sel = $("llm-site");
      sel.innerHTML = "";
      for (const p of sitePresets) {
        const o = document.createElement("option");
        o.value = p.id; o.textContent = p.base_url ? p.name : `${p.name}（手填 base_url）`;
        sel.appendChild(o);
      }
    }
  } catch (e) { /* 软依赖：拉不到则站点栏留空不阻塞 */ }
}
function siteById(id) { return sitePresets.find((p) => p.id === id); }
function curSiteId() {
  const base = $("llm-base").value.trim().replace(/\/+$/, "");
  const hit = sitePresets.find((p) => p.base_url && p.base_url.replace(/\/+$/, "") === base);
  return hit ? hit.id : (sitePresets.length ? "custom" : "");
}
function applySite(id) {
  const p = siteById(id);
  if (!p) { $("llm-site-key-url").hidden = true; return; }
  if (p.base_url) {
    $("llm-base").value = p.base_url;
    const saved = localStorage.getItem("react-agent:llmkey:" + p.id) || "";
    $("llm-key").value = saved;
    $("llm-key").placeholder = saved ? `已记忆该站点 key（尾号 ${saved.slice(-4)}，可覆盖）` : "未保存过该站点 key";
  } else {
    $("llm-base").value = ""; $("llm-key").value = "";
    $("llm-key").placeholder = "输入该站点 api_key";
  }
  updateSiteLink(id);
}
function updateSiteLink(id) {
  const p = siteById(id), link = $("llm-site-key-url");
  if (p && p.key_url) { link.href = p.key_url; link.hidden = false; } else link.hidden = true;
}
$("llm-site").addEventListener("change", () => applySite($("llm-site").value));
function syncLlmFields() {
  const p = $("llm-provider").value;
  const baseWrap = $("llm-base").closest(".field");
  const baseLabel = baseWrap.querySelector("label");
  const keyWrap = $("llm-key").closest(".field");
  const siteWrap = $("llm-site-field");
  if (p === "ollama") {
    baseLabel.textContent = "ollama 地址";
    $("llm-base").placeholder = "localhost:11434";
    const bv = $("llm-base").value.trim();
    if (!bv || /api\.(openai|anthropic|deepseek)\.com/.test(bv)) $("llm-base").value = "localhost:11434";
    keyWrap.style.display = "none";
    baseWrap.style.display = "";
    siteWrap.style.display = "none";
  } else if (p === "mock") {
    baseWrap.style.display = "none";
    keyWrap.style.display = "none";
    siteWrap.style.display = "none";
  } else {
    baseLabel.textContent = "base_url";
    $("llm-base").placeholder = p === "anthropic" ? "https://api.anthropic.com" : "https://api.deepseek.com/v1";
    baseWrap.style.display = "";
    keyWrap.style.display = "";
    siteWrap.style.display = p === "openai" ? "" : "none"; // 站点预设仅 OpenAI 兼容族
  }
}
$("llm-save").onclick = async () => {
  const msel = $("llm-model");
  const model = msel.value === "__custom__" ? $("llm-model-custom").value.trim() : msel.value.trim();
  const body = { llm: { provider: $("llm-provider").value, model, base_url: $("llm-base").value.trim() } };
  const key = $("llm-key").value.trim();
  if (key) body.llm.api_key = key;
  try {
    await api("PUT", "/api/config", body);
    // per-site key 记忆：按站点 id 存 localStorage（custom 不记，base_url 不定）；config.json 只落当前生效站
    if (key && $("llm-provider").value === "openai") {
      const sid = $("llm-site").value;
      if (sid && sid !== "custom") localStorage.setItem("react-agent:llmkey:" + sid, key);
    }
    flash("llm-flash", true, "已保存并热生效");
    loadConfig();
    fetchModels();
  } catch (e) { flash("llm-flash", false, e.message); }
};
// 工具三池分组元数据（pool → 组标题/说明）；条目点击折叠展开配置详情
const POOL_META = {
  builtin: { label: "内置工具", hint: "" },
  skill: { label: "技能工具", hint: "随技能装载（tools.json 声明）——装载 ≠ 启用" },
  mcp: { label: "MCP 外接", hint: "展开各服务组：勾选启用工具、填写 API Key（保存落盘，重启 host 生效）" },
};
function paramSummary(p) {
  if (!p || typeof p !== "object") return "";
  const props = p.properties;
  if (!props || typeof props !== "object") return "";
  const lines = Object.entries(props).map(([k, v]) => {
    const t = v && v.type ? v.type : "?";
    const d = v && v.description ? ` — ${v.description}` : "";
    return `${k} (${t})${d}`;
  });
  return lines.length ? lines.join("\n") : "（无参数）";
}
function renderTools() {
  const box = $("tools-list"); box.innerHTML = "";
  if (!cfgTools.length) { box.innerHTML = '<div class="empty">（工具池为空或 tools 不可用）</div>'; return; }
  const groups = { builtin: [], skill: [], mcp: [] };
  for (const t of cfgTools) (groups[t.pool] ?? groups.builtin).push(t);
  for (const pool of ["builtin", "skill", "mcp"]) {
    const items = groups[pool];
    if (!items.length && !(pool === "mcp" && (cfgMcp.servers ?? []).length)) continue; // MCP 池：声明了 server（即使装载失败 0 工具）也要可见
    const meta = POOL_META[pool];
    // 池级折叠：poolhead 本身可点击收起/展开整池（默认展开）
    const phead = el(`<div class="poolhead collapsible"><span class="chev">▾</span><span class="plabel">${meta.label}</span><span class="pcnt">${items.length}</span></div>`);
    const pbody = el(`<div class="poolbody"></div>`);
    phead.onclick = () => {
      pbody.hidden = !pbody.hidden;
      phead.querySelector(".chev").textContent = pbody.hidden ? "▸" : "▾";
      phead.classList.toggle("closed", pbody.hidden);
    };
    box.appendChild(phead);
    box.appendChild(pbody);
    if (meta.hint) pbody.appendChild(el(`<div class="poolhint">${meta.hint}</div>`));
    if (pool === "builtin") {
      // 内置 9 件：少而稳定，平铺
      for (const t of items) appendToolRow(pbody, t, "");
      continue;
    }
    // 技能/MCP 池：按来源折叠分组（默认收起），组头 = 来源名 + 计数 +（MCP）状态灯 + 总开关
    const key = pool === "skill" ? "skill" : "mcp_server";
    const order = new Map();
    if (pool === "mcp") {
      // server 排序：ready 在前、其余按名稳定排；failed 垫底（故障一眼可见）
      const rank = { ready: 0, starting: 1, stopped: 1, failed: 2 };
      (cfgMcp.servers ?? []).forEach((s) => order.set(s.name, s));
      items.sort((a, b) => ((rank[order.get(a[key])?.status] ?? 1) - (rank[order.get(b[key])?.status] ?? 1)) || a[key].localeCompare(b[key]));
    } else {
      items.sort((a, b) => (a[key] ?? "").localeCompare(b[key] ?? ""));
    }
    const bySrc = new Map();
    for (const t of items) { const k = t[key] ?? "?"; (bySrc.get(k) ?? bySrc.set(k, []).get(k)).push(t); }
    if (pool === "mcp") {
      // 装载失败/0 工具的声明 server 也要可见（否则用户无从发现该去填 key）
      for (const s of cfgMcp.servers ?? []) if (!bySrc.has(s.name)) bySrc.set(s.name, []);
    }
    for (const [src, list] of bySrc) {
      const st = order.get(src);
      const stHtml = pool === "mcp" && st
        ? `<span class="mcp-status st-${esc(st.status)}" ${st.status === "failed" && st.error ? `title="${esc(st.error)}"` : ""}>${esc(st.status)}</span>`
        : "";
      const head = el(`<div class="tghead"><input type="checkbox" title="全启/全停该组工具"><span class="tname">${esc(src)}</span><span class="pcnt">${list.length}</span>${stHtml}<span class="chev">▸</span></div>`);
      const body = el(`<div class="tgbody" hidden></div>`);
      for (const t of list) appendToolRow(body, t, pool === "skill" ? "" : `<span class="badge b-skill" title="MCP server：${esc(src)}">mcp</span>`);
      if (pool === "mcp" && !list.length) body.appendChild(el(`<div class="empty">0 工具装载——${esc(st?.error || st?.status || "未连接")}；下方填好 Key 保存后重启 host 重试</div>`));
      // MCP 服务配置区（组内）：env key 输入 + 独立保存（仅落盘，重启 host 生效）
      if (pool === "mcp") {
        const decl = (cfgMcp.declared ?? []).find((d) => d.name === src);
        if (decl && (decl.env ?? []).length) {
          const cfgBox = el(`<div class="tgcfg"><div class="tgcfg-title">服务配置（Key 保存仅落盘，需重启 host 生效）</div></div>`);
          for (const e of decl.env) {
            cfgBox.appendChild(el(`<div class="field"><label>${esc(e.key)}</label><input type="password" autocomplete="off" data-k="${esc(e.key)}" placeholder="${e.key_set ? `已设置（尾号 ${esc(e.key_tail ?? "")}，留空不修改）` : "未设置"}"></div>`));
          }
          const srow = el(`<div class="row"><button class="btn">保存 Key</button><span class="flash"></span></div>`);
          const sflash = srow.querySelector(".flash");
          srow.querySelector("button").onclick = async (e) => {
            e.stopPropagation();
            const env = {};
            for (const inp of cfgBox.querySelectorAll("input[data-k]")) {
              const v = inp.value.trim();
              if (v) { env[inp.dataset.k] = v; inp.value = ""; }
            }
            if (!Object.keys(env).length) { sflash.textContent = "无修改（留空 = 不覆盖）"; sflash.className = "flash err"; return; }
            try {
              await api("PUT", "/api/config", { mcp_servers: { [src]: { env } } });
              sflash.textContent = "已保存，重启 host 生效"; sflash.className = "flash ok";
            } catch (err) { sflash.textContent = err.message; sflash.className = "flash err"; }
          };
          cfgBox.appendChild(srow);
          body.appendChild(cfgBox);
        }
      }
      // 总开关三态：all / none / partial(indeterminate)；组内勾选变化实时回写
      const syncHead = () => {
        const boxes = [...body.querySelectorAll(".toolitem input")];
        const on = boxes.filter((b) => b.checked).length;
        head.querySelector("input").checked = on > 0 && on === boxes.length;
        head.querySelector("input").indeterminate = on > 0 && on < boxes.length;
      };
      head.querySelector("input").onclick = (e) => {
        e.stopPropagation();
        const on = head.querySelector("input").checked;
        body.querySelectorAll(".toolitem input").forEach((b) => { b.checked = on; });
        syncHead();
      };
      body.addEventListener("change", syncHead);
      head.onclick = (e) => {
        e.stopPropagation(); // 组头点击不冒泡到池级折叠
        if (e.target.tagName === "INPUT") return;
        body.hidden = !body.hidden;
        head.querySelector(".chev").textContent = body.hidden ? "▸" : "▾";
        head.classList.toggle("open", !body.hidden);
      };
      syncHead();
      pbody.appendChild(head);
      pbody.appendChild(body);
    }
  }
}
function appendToolRow(box, t, src) {
  const params = paramSummary(t.parameters);
  const row = el(`<div class="toolitem"><input type="checkbox" ${t.enabled ? "checked" : ""}><span class="name">${esc(t.name)}</span>${src}<span class="chev">▸</span></div>`);
  const detail = (t.description || params)
    ? el(`<div class="tooldetail" hidden><div class="tdesc">${esc(t.description ?? "")}</div>${params ? `<pre class="tparams">${esc(params)}</pre>` : ""}</div>`)
    : null;
  row.onclick = (e) => {
    if (e.target.tagName === "INPUT") return; // 勾选不触发折叠
    if (!detail) return;
    const open = detail.hidden;
    detail.hidden = !open;
    row.querySelector(".chev").textContent = open ? "▾" : "▸";
    row.classList.toggle("open", open);
  };
  box.appendChild(row);
  if (detail) box.appendChild(detail);
}
const toolsTabObserver = new MutationObserver(() => { if (!$("tab-tools").hidden) renderTools(); });
toolsTabObserver.observe($("tab-tools"), { attributes: true, attributeFilter: ["hidden"] });
$("tools-save").onclick = async () => {
  // 整体替换白名单：三池条目已统一渲染于 #tools-list，勾选合并提交
  // 注意选择器限定 .toolitem——折叠组头也有 checkbox（总开关），不能混入
  // MCP key 保存已移入各服务组内独立按钮（tools-save 只管白名单）
  const enabled = [...document.querySelectorAll("#tools-list .toolitem input:checked")].map((i) => i.closest(".toolitem").querySelector(".name").textContent);
  const body = { tools: { enabled } };
  try {
    await api("PUT", "/api/config", body);
    flash("tools-flash", true, "已保存，白名单热生效");
    await loadConfig();
  } catch (e) { flash("tools-flash", false, e.message); }
};

// P5/E1 Agent 参数：空字段 = 不修改；system_prompt 总是提交（允许清空回内置链）
$("agent-save").onclick = async () => {
  const agent = {};
  for (const [k, id] of [
    ["max_rounds", "agent-max-rounds"], ["output_dir", "agent-output-dir"], ["budget_secs", "agent-budget-secs"], ["token_budget", "agent-token-budget"],
    ["history_limit", "agent-history-limit"], ["compact_trigger", "agent-compact-trigger"], ["compact_keep", "agent-compact-keep"],
    ["tool_result_limit", "agent-tool-result-limit"], ["retry_attempts", "agent-retry-attempts"], ["retry_base_ms", "agent-retry-base-ms"],
    ["llm_context_tokens", "agent-llm-context-tokens"],
  ]) {
    const raw = $(id).value.trim();
    if (raw !== "") agent[k] = Number(raw);
  }
  agent.system_prompt = $("agent-system-prompt").value;
  try {
    await api("PUT", "/api/config", { agent });
    flash("agent-flash", true, "已保存，下轮对话生效");
    loadConfig();
  } catch (e) { flash("agent-flash", false, e.message); }
};

let skillsCache = [], curSkill = null;
async function loadSkills() {
  try {
    skillsCache = (await api("GET", "/api/skills")).skills ?? [];
    renderSkills();
  } catch (e) { flash("skill-flash", false, "加载失败: " + e.message); }
}
function renderSkills() {
  const box = $("skills-list"); box.innerHTML = "";
  if (!skillsCache.length) box.innerHTML = '<div class="empty">（暂无技能）</div>';
  for (const s of skillsCache) {
    // 来源徽章：origin=preset 出厂（删除保护）/ 缺省 user 用户；老数据缺 origin 按 user 显示
    const isUser = s.origin !== "preset";
    const badge = `<span class="badge ${isUser ? "b-user" : "b-preset"}" title="${isUser ? "用户技能（可删除）" : "出厂技能（删除保护）"}">${isUser ? "用户" : "内置"}</span>`;
    const item = el(`<div class="skillitem"><span class="name">${esc(s.name)}</span>${badge}<span class="desc">${esc(s.description ?? "")}</span><span class="rev" title="在文件管理器中打开技能目录">📂</span></div>`);
    item.onclick = () => openSkill(s.name);
    item.querySelector(".rev").onclick = (e) => { e.stopPropagation(); reveal("skill", s.name, "skill-flash"); };
    box.appendChild(item);
    // R9/W6：配套工具区（tools_detail 由 host 从 tools.skill_tools 合入，含未启用项 + enabled 标记）。
    // 启停就地完成：永远走配置闸（现有启用集 ± 该工具 → PUT /api/config）；失败回滚 UI。
    const td = s.tools_detail ?? [];
    if (!td.length) continue;
    const zone = el(`<div class="skilltools"></div>`);
    for (const t of td) {
      const row = el(`<label class="skilltool"><input type="checkbox" ${t.enabled ? "checked" : ""}><span class="sname">${esc(t.name)}</span><span class="sdesc">${esc(t.description ?? "")}</span></label>`);
      row.querySelector("input").onchange = async (e) => {
        const on = e.target.checked;
        try {
          const c = (await api("GET", "/api/config")).config;
          const set = new Set((c.tools ?? []).filter((x) => x.enabled).map((x) => x.name));
          if (on) set.add(t.name); else set.delete(t.name);
          await api("PUT", "/api/config", { tools: { enabled: [...set] } });
          flash("skill-flash", true, on ? `已启用 ${t.name}（会话中 load_skill 后生效）` : `已禁用 ${t.name}`);
          loadConfig();
        } catch (err) {
          e.target.checked = !on; // 启用失败回滚勾选态
          flash("skill-flash", false, err.message);
        }
      };
      zone.appendChild(row);
    }
    box.appendChild(zone);
  }
}
// ── 产物文件卡片：agent 产出的成品文件（artifact trace 事件）→ 可点击查看/下载 ──
// 点击分流：文本类内嵌预览（md 走富渲染）；html/图片/pdf 浏览器原生渲染新标签打开；
// office/zip 等二进制走 /files?download=1 下载。来源（相对路径+工具）展示在卡片上。
const PREVIEW_TEXT_EXTS = ["md", "markdown", "txt", "log", "json", "csv", "js", "ts", "py", "rs", "css", "xml", "yml", "yaml"];
const PREVIEW_NATIVE_EXTS = ["html", "htm", "svg", "png", "jpg", "jpeg", "gif", "webp", "pdf"];
function filesUrl(p, download) {
  const encoded = p.split(/[\\/]/).map(encodeURIComponent).join("/");
  return "/files/" + encoded + (download ? "?download=1" : "");
}
function renderArtifactCard(ev) {
  if (!ev.path) return;
  const key = ev.path.replace(/\\/g, "/").toLowerCase();
  if (turnArtifactPaths.has(key)) return; // 同回合同路径去重
  turnArtifactPaths.add(key);
  const name = ev.path.split(/[\\/]/).pop();
  const ext = (name.includes(".") ? name.split(".").pop() : "").toLowerCase();
  const card = el(`<div class="artcard"><span class="ico">📄</span><span class="nm">${esc(name)}</span><span class="src" title="${esc(ev.path)}">${esc(ev.path)}${ev.tool ? " · " + esc(ev.tool) : ""}</span><span class="open">${PREVIEW_TEXT_EXTS.includes(ext) || PREVIEW_NATIVE_EXTS.includes(ext) ? "查看" : "下载"}</span></div>`);
  card._artpath = key; // R12：最终答案「提及过滤」按此键匹配（文件名命中或全路径命中）
  card.querySelector(".open").onclick = () => openWsFile(ev.path); // W14：与文件树共用一条预览链
  ensureTurnTail().appendChild(card);
  turnArtifacts.push(card); // 回合收敛时移到最终答案之后置底
  scrollFeed();
}
// ── 来源 chip + 侧边抽屉（信息溯源）：web_search/web_read 的引用链接折叠为一张
// 「🔗 来源 (N)」小卡；点击从右侧滑出抽屉逐条查看。回合内一张 chip 累计去重，
// 引用清单挂在 chip 元素上（置底移动后仍可随时点开回看）。
function renderSourcesCard(ev) {
  const items = (ev.items ?? []).filter((it) => it && it.url && !turnSourceUrls.has(it.url));
  if (!items.length) return;
  const chip = turnSourceBox ?? el(`<div class="srcchip" title="点击查看引用来源">🔗 来源 <span class="cnt">0</span></div>`);
  chip._items = chip._items ?? [];
  for (const it of items) {
    turnSourceUrls.add(it.url);
    chip._items.push(it);
  }
  chip.querySelector(".cnt").textContent = chip._items.length;
  chip.onclick = () => openSourceDrawer(chip._items);
  if (!turnSourceBox) {
    ensureTurnTail().appendChild(chip);
    turnSourceBox = chip;
    scrollFeed();
  }
}
function openSourceDrawer(items) {
  let d = $("src-drawer");
  if (!d) {
    d = el(`<div id="src-drawer" class="sd-overlay"><div class="sd-panel"><div class="sd-head"><span>🔗 引用来源</span><button class="btn ghost sd-close">关闭</button></div><div class="sd-body"></div></div></div>`);
    document.body.appendChild(d);
    d.onclick = (e) => { if (e.target === d) d.classList.remove("open"); };
    d.querySelector(".sd-close").onclick = () => d.classList.remove("open");
  }
  const body = d.querySelector(".sd-body");
  body.innerHTML = "";
  (items ?? []).forEach((it, i) => {
    body.appendChild(el(`<div class="sd-item"><span class="sd-idx">${i + 1}</span><a href="${esc(it.url)}" target="_blank" rel="noopener noreferrer">${esc(it.title || it.url)}</a><span class="u">${esc(it.url)}</span></div>`));
  });
  d.classList.add("open");
}
// 回合收敛：产物卡与来源卡移到最终答案之后（appendChild 对既有节点即移动）。
// 回合异常终止（无 assistant 收敛事件）时卡片留在过程框内，仍可见。
function settleTurnEnd() {
  for (const c of turnArtifacts) feed.appendChild(c);
  turnArtifacts = [];
  if (turnSourceBox) feed.appendChild(turnSourceBox);
  turnSourceBox = null;
  if (turnFcBox) feed.appendChild(turnFcBox); // 📝 文件变更 chip 同款置底
  turnFcBox = null;
  turnTail?.remove();
  turnTail = null;
}
// R12 产物「提及过滤」：只保留最终答案点名的文件——write_file 的中间草稿等与
// 交付无关的成品不再铺开。文件名或完整路径出现在答案文本中即算提及；答案一条
// 都没点名 → 不启用过滤（避免误杀「生成了但没逐个点名」的场景）。
function filterArtifactsByAnswer(answer) {
  if (!turnArtifacts.length || !answer) return;
  const hay = String(answer).replace(/\\/g, "/").toLowerCase();
  const kept = turnArtifacts.filter((c) => {
    const p = c._artpath ?? "";
    return p && (hay.includes(p) || hay.includes(p.split("/").pop() ?? "\u0000"));
  });
  if (!kept.length || kept.length === turnArtifacts.length) return;
  for (const c of turnArtifacts) if (!kept.includes(c)) c.remove();
  turnArtifacts = kept;
}
// ── Monaco 代码预览（W11）：本地 vendor → CDN → 纯文本 三级链 ──
// 先探测 loader.js 可达性（GET，仅 ~10KB）再注入，保证同页只加载一个 AMD loader，
// 规避双 loader 注册表互踩；editor.main 装载失败则调用方退回纯文本兜底。
const MONACO_LANGS = { js: "javascript", ts: "typescript", py: "python", rs: "rust", css: "css", xml: "xml", yml: "yaml", yaml: "yaml", json: "json", csv: "plaintext", txt: "plaintext", log: "plaintext" };
let monacoReady = null, previewEditor = null, previewDiff = null;
function disposePreviewEditor() {
  if (previewEditor) { try { previewEditor.dispose(); } catch {} previewEditor = null; }
  if (previewDiff) {
    try { previewDiff.models.forEach((m) => m.dispose()); } catch {}
    try { previewDiff.ed.dispose(); } catch {}
    previewDiff = null;
  }
}
function tryMonacoBase(base) {
  // 探测 + 注入：fetch loader.js（CDN 需 CORS，jsdelivr 支持；本地同源无碍）
  return fetch(base + "/loader.js", { cache: "no-store" })
    .then((r) => (r.ok ? new Promise((resolve) => {
      const s = document.createElement("script");
      s.src = base + "/loader.js";
      s.onload = () => { try { require.config({ paths: { vs: base } }); require(["vs/editor/editor.main"], () => resolve(true), () => resolve(false)); } catch { resolve(false); } };
      s.onerror = () => resolve(false);
      document.head.appendChild(s);
    }) : false))
    .catch(() => false);
}
function ensureMonaco() {
  if (monacoReady) return monacoReady;
  monacoReady = (async () => {
    // 一级：本地 vendor（同源）；二级：CDN 回退。两种基座统一用「绝对 baseUrl + blob 代理
    // worker」——tsWorker 等 labored worker 在 worker 作用域内解析相对 URL 会炸
    // （Failed to parse URL），绝对地址是官方推荐写法。
    const base = (await tryMonacoBase("vendor/monaco/vs")) ? "vendor/monaco/vs"
      : ((await tryMonacoBase("https://cdn.jsdelivr.net/npm/monaco-editor@0.52.2/min/vs")) ? "https://cdn.jsdelivr.net/npm/monaco-editor@0.52.2/min/vs" : null);
    if (base) {
      const abs = new URL(base, location.href).href.replace(/\/$/, "");
      // MonacoEnvironment.baseUrl 指向 vs/ 的父目录（模块 id 自带 vs/ 前缀），别多带一层
      const absRoot = abs.replace(/\/vs$/, "");
      window.MonacoEnvironment = {
        getWorkerUrl: () => URL.createObjectURL(new Blob(
          [`self.MonacoEnvironment={baseUrl:'${absRoot}/'};importScripts('${abs}/base/worker/workerMain.js');`],
          { type: "text/javascript" })),
      };
      return true;
    }
    return false; // 三级：调用方退回纯文本
  })();
  return monacoReady;
}
function ensurePreviewOverlay() {
  let overlay = $("file-preview");
  if (!overlay) {
    overlay = el(`<div id="file-preview" class="fp-overlay"><div class="fp-box"><div class="fp-head"><span class="fp-name"></span><button class="btn ghost fp-close">关闭</button></div><div class="fp-body"></div></div></div>`);
    document.body.appendChild(overlay);
    overlay.onclick = (e) => { if (e.target === overlay) { disposePreviewEditor(); overlay.style.display = "none"; } };
    overlay.querySelector(".fp-close").onclick = () => { disposePreviewEditor(); overlay.style.display = "none"; };
  }
  return overlay;
}
let previewSeq = 0; // 预览时序令牌：连续切换文件时丢弃迟到的旧响应，防慢请求回写覆盖新预览
async function previewTextFile(path, ext) {
  const seq = ++previewSeq;
  try {
    const r = await fetch(filesUrl(path));
    if (!r.ok) throw new Error("HTTP " + r.status);
    const text = await r.text();
    if (seq !== previewSeq) return; // 已有更新的预览请求，本次结果作废
    const overlay = ensurePreviewOverlay();
    overlay.querySelector(".fp-name").textContent = path;
    const body = overlay.querySelector(".fp-body");
    disposePreviewEditor();
    if (ext === "md" || ext === "markdown") {
      body.innerHTML = md(text);
    } else if (await ensureMonaco()) {
      body.innerHTML = `<div id="fp-monaco" style="height:100%"></div>`;
      previewEditor = monaco.editor.create($("fp-monaco"), {
        value: text, language: MONACO_LANGS[ext] || "plaintext", readOnly: true,
        minimap: { enabled: false }, automaticLayout: true, fontSize: 13, scrollBeyondLastLine: false,
      });
    } else {
      body.innerHTML = `<pre>${esc(text)}</pre>`;
    }
    overlay.style.display = "flex";
  } catch (e) { setStatus("文件预览失败: " + e.message); }
}

// ── 变更对比（R15）：数据源三级降级——① 变更快照（/api/fc-snapshot，精确历史双栏）；
// ② edit 无快照时用工具卡 args（old/new）对当前内容反向重构（replace_all 全量替换）；
// ③ 重构不了（write 覆盖无快照等）仅单侧显示当前内容。Monaco DiffEditor 双栏对比，
// 本地 vendor 不可达时退化为上下排布的两段 <pre>。
async function fetchSnap(id, side) {
  const r = await fetch(`/api/fc-snapshot?id=${encodeURIComponent(id)}&side=${side}`);
  if (!r.ok) throw new Error("HTTP " + r.status);
  return r.text();
}
async function openFcDiff(it) {
  const seq = ++previewSeq;
  const overlay = ensurePreviewOverlay();
  overlay.querySelector(".fp-name").textContent = `${it.path} · 变更对比`;
  const body = overlay.querySelector(".fp-body");
  disposePreviewEditor();
  body.innerHTML = '<div class="wsp-empty">加载中…</div>';
  overlay.style.display = "flex";
  let before = null, after = null, note = "";
  try {
    if (it.undo?.id) {
      before = it.undo.created ? "" : await fetchSnap(it.undo.id, "before");
      after = it.undo.deleted ? null : await fetchSnap(it.undo.id, "after");
      if (it.undo.deleted) note = "文件在该次脚本执行中被删除，仅显示删除前完整内容（回滚可还原）";
    } else {
      const r = await fetch(filesUrl(it.path));
      if (!r.ok) throw new Error("HTTP " + r.status);
      after = await r.text();
      if (it.op === "edit") {
        // 降级②：从工具卡暂存的 args 反向重构变更前内容
        const cards = [...document.querySelectorAll("#feed details.tool")].filter(
          (c) => c._fcpath === it.path.toLowerCase() && String(c._fcround) === String(it.round) && !it.sub === !c._fcsub
        );
        const a = cards[cards.length - 1]?._fcargs;
        if (a && typeof a.old_string === "string" && typeof a.new_string === "string" && after.includes(a.new_string)) {
          before = a.replace_all ? after.split(a.new_string).join(a.old_string) : after.replace(a.new_string, a.old_string);
        }
        if (before == null) note = "无变更快照且无法从编辑参数精确重构，仅显示当前内容";
      } else {
        note = "覆盖写入且无变更快照，变更前内容不可重建，仅显示当前内容";
      }
    }
  } catch (e) {
    if (seq === previewSeq) body.innerHTML = `<div class="wsp-empty">变更内容加载失败: ${esc(e.message)}</div>`;
    return;
  }
  if (seq !== previewSeq) return; // 已有更新的预览请求，本次作废
  const ext = (it.path.includes(".") ? it.path.split(".").pop() : "").toLowerCase();
  const lang = MONACO_LANGS[ext] || "plaintext";
  const noteHtml = note ? `<div class="fp-note">${esc(note)}</div>` : "";
  if (before != null && after != null && (await ensureMonaco())) {
    body.innerHTML = `${noteHtml}<div id="fp-monaco" style="height:${note ? "calc(100% - 30px)" : "100%"}"></div>`;
    const orig = monaco.editor.createModel(before, lang);
    const mod = monaco.editor.createModel(after, lang);
    const de = monaco.editor.createDiffEditor($("fp-monaco"), {
      readOnly: true, renderSideBySide: true, automaticLayout: true,
      fontSize: 13, minimap: { enabled: false }, scrollBeyondLastLine: false,
    });
    de.setModel({ original: orig, modified: mod });
    previewDiff = { ed: de, models: [orig, mod] };
  } else if (before != null) {
    if (after != null) {
      body.innerHTML = `${noteHtml}<div class="fp-note">变更前</div><pre>${esc(before)}</pre><div class="fp-note">变更后</div><pre>${esc(after)}</pre>`;
    } else {
      body.innerHTML = `${noteHtml}<div class="fp-note">删除前</div><pre>${esc(before)}</pre>`;
    }
  } else if (await ensureMonaco()) {
    body.innerHTML = `${noteHtml}<div id="fp-monaco" style="height:${note ? "calc(100% - 30px)" : "100%"}"></div>`;
    previewEditor = monaco.editor.create($("fp-monaco"), {
      value: after, language: lang, readOnly: true,
      minimap: { enabled: false }, automaticLayout: true, fontSize: 13, scrollBeyondLastLine: false,
    });
  } else {
    body.innerHTML = `${noteHtml}<pre>${esc(after)}</pre>`;
  }
}

async function openSkill(name) {
  // toggle：再点当前已展开的技能条目 → 收起编辑器
  if (curSkill === name && !$("skill-editor").hidden) {
    $("skill-editor").hidden = true;
    curSkill = null;
    flash("skill-flash", true, "已收起 " + name);
    return;
  }
  try {
    const v = await api("GET", "/api/skills/" + encodeURIComponent(name));
    curSkill = name;
    $("skill-editor").hidden = false;
    $("skill-name").value = name; $("skill-name").readOnly = true;
    $("skill-content").value = v.content ?? "";
    // 来源删除保护：出厂件（origin=preset）删除置灰（后端仍有一道保护，此处仅 UI 提示）
    const isUser = (skillsCache.find((x) => x.name === name)?.origin ?? "user") === "user";
    $("skill-del").hidden = false;
    $("skill-del").disabled = !isUser;
    $("skill-del").title = isUser ? "删除该用户技能" : "出厂技能不可删除（origin=preset）";
    flash("skill-flash", true, "已加载 " + name);
  } catch (e) { flash("skill-flash", false, e.message); }
}
// ── 设置面板「📂 源文件」：host 代为在文件管理器中打开配置源文件位置（浏览器禁 file:// 跳转）──
async function reveal(target, name, flashId) {
  try {
    await api("POST", "/api/reveal?target=" + target + (name ? "&name=" + encodeURIComponent(name) : ""));
    flash(flashId, true, "已在文件管理器中打开");
  } catch (e) { flash(flashId, false, e.message); }
}
$("llm-reveal").onclick = () => reveal("config", null, "llm-flash");
// ── W14 工作区文件树入口（头部 📁）──
$("btn-tree").onclick = toggleWsTree;
$("tools-reveal").onclick = () => reveal("tools", null, "tools-flash");
$("skills-reveal").onclick = () => reveal("skills", null, "skill-flash");
$("skill-new").onclick = () => {
  curSkill = null;
  $("skill-editor").hidden = false;
  $("skill-name").value = ""; $("skill-name").readOnly = false;
  $("skill-content").value = "---\nname: \ndescription: \norigin: user\n---\n\n# 何时使用\n…\n\n# 执行指引\n…";
  $("skill-del").hidden = true;
  $("skill-del").disabled = false;
  $("skill-name").focus();
};
$("skill-save").onclick = async () => {
  const name = $("skill-name").value.trim(), content = $("skill-content").value;
  if (!name) return flash("skill-flash", false, "缺名称");
  try {
    await api("PUT", "/api/skills/" + encodeURIComponent(name), { content });
    flash("skill-flash", true, "已保存，下轮对话可见");
    curSkill = name; $("skill-name").readOnly = true; $("skill-del").hidden = false;
    loadSkills();
  } catch (e) { flash("skill-flash", false, e.message); }
};
$("skill-del").onclick = async () => {
  if (!curSkill || !confirm("删除技能 " + curSkill + "？")) return;
  try {
    await api("DELETE", "/api/skills/" + encodeURIComponent(curSkill));
    flash("skill-flash", true, "已删除");
    $("skill-editor").hidden = true; loadSkills();
  } catch (e) { flash("skill-flash", false, e.message); }
};

"use strict";
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


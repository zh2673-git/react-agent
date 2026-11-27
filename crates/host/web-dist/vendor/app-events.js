"use strict";
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
      // X1：普通回滚也把本轮来源传进去——预填需要原文，索引只能定位不能恢复文字。
      const _turn = userTurns[userTurns.length - 1];
      uel.querySelector(".rb-btn").onclick = () => rollbackTo(idx, false, _turn);
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
let es = null, lastAfter = 0, reconnectTimer = null;

// P7 会话重置集中化：首屏/切换会话/回滚后的全量清场，过去是 connect(0) 里一行 20+ 变量的
// 手工链（跨模块直改 app-stream/app-files 内部状态），任何一处漏登记就是串台 bug。
// 现在各模块只暴露一个 reset 入口，这里按依赖序调用；新增状态在本模块 reset 内登记。
function resetSessionFeed() {
  feed.innerHTML = "";
  pending.clear();
  lastAfter = 0;
  preciseCursor = false;             // Q 轮：重放回到近似计数口径，收到 cursor 帧后才转精确
  doneSids.clear();
  resetStreamState();   // app-stream.js：流式句柄 + 过程框 + 回合卡 + 子代理框 + 缓冲
  resetFileChanges();   // app-files.js：文件变更视图
  userTurns = [];
  turnArtifacts = [];
  turnSourceBox = null;
  turnSourceUrls.clear();
  turnArtifactPaths.clear();
  turnTail = null;
  turnFcBox = null;
}

function connect(after, useCache) {
  const cached = useCache && after === undefined ? restoreSessionFeed(session) : null;
  after = after || (cached ? cached.cursor : 0);
  if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
  if (es) es.close();
  if (!cached && after === 0) resetSessionFeed(); // 首屏/切换/回滚后：集中重置（P7），清空后全量重放
  // 清 DOM 后流式句柄一并失效，避免继续写入已移除的气泡
  if (!cached) streamSid = streamCard = streamBody = streamStats = null;
  es = new EventSource(`/api/events?session=${encodeURIComponent(session)}&after=${after}`);
  bindQueueUI(); syncRunUI();
  es.onopen = () => { connEl.classList.add("live"); setStatus(""); };
  // onerror 同时会收到「连接错误」和「服务端 event: error 帧」：带 data 的是业务错误帧，
  // 只渲染横幅，绝不能在这里重连——否则每条 trace.read 失败都会关连接重建、清空 feed，
  // 页面表现为持续闪烁。真断线则延迟 1s 用当前游标续传（保留已渲染内容，防重连风暴）。
  es.onerror = (e) => {
    if (e && typeof e.data === "string" && e.data) {
      let ev; try { ev = JSON.parse(e.data); } catch { return; }
      try { render(ev); } catch (err) { console.warn("render failed:", ev?.type, err); }
      return;
    }
    connEl.classList.remove("live"); setStatus("连接断开，重连中…");
    if (reconnectTimer) return; // 已有排队重连：避免同一故障触发多次重建
    const dead = es;
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      if (es === dead) connect(lastAfter);
    }, 1000);
  };
  es.onmessage = (m) => {
    let ev; try { ev = JSON.parse(m.data); } catch { return; }
    // Q 轮：服务端帧带精确 trace 游标（after + 本批序号）。优先用之——
    // 跨批/跳帧时前端自增计数会漂，切回缓存会话就会重放重复气泡。
    if (ev && typeof ev.cursor === "number" && ev.cursor >= 0) {
      lastAfter = ev.cursor; preciseCursor = true;
    } else if (ev && ev.type && !String(ev.type).startsWith("stream_") && !preciseCursor) {
      lastAfter++; // 无 cursor 字段的旧服务端：退回自增近似
    }
    if (ev) delete ev.cursor;           // 渲染层不感知游标字段
    try { render(ev); } catch (e) { console.warn("render failed:", ev?.type, e); } // 单事件渲染异常不拖垮游标与后续事件
  };
}
connect();
// W17：实例名徽章随启动渲染（loadConfig 仅在打开设置/保存时触发，启动路径须单独补）
api("GET", "/api/config").then((v) => showInstBadge(v.config?.instance_name ?? "")).catch(() => {});
// W17：header 显示当前工作区文件夹名（主窗口启动即绑定工作区，选新文件夹只在「新窗口」时发生）
api("GET", "/api/tree?limit=1").then((t) => {
  const name = String(t.root || "").replace(/\\\\\?\\/, "").replace(/[\\/]+$/, "").split(/[\\/]/).pop();
  if (name) { const el = $("ws-tag"); el.textContent = "· 📁 " + name; el.hidden = false; }
}).catch(() => {});

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

// Q 轮：发送 = 入队 + 单消费者。运行中连发只入队、不发 fetch；每条在 drain 的
// finally 之后才取下一条，绝不与当前 chat 抢同会话闸。停止 = cancel + 队列退回输入框。
async function send(presetText, presetAtts) {
  const text = (presetText ?? input.value).trim();
  if (!text) return;
  const atts = presetAtts ? presetAtts : pendingAtts.splice(0);
  if (presetText === undefined) { input.value = ""; input.style.height = "48px"; renderAtts(); }
  touchSession(session);
  autoName(session, text); // 标题为空时按首条消息自动命名
  const s = qState(session);
  // Q7：用户又发新消息 = 明确要「继续干活」，解除上一轮停止留下的暂留标记，
  // 队列按原序恢复（新消息排在最后）。不解除的话 send 之后队列还是停着，
  // 用户看到「我发了新指令却什么都没发生」。
  s.queue.forEach((it) => { it.halted = false; });
  s.queue.push({ text, atts });
  queueOpen = true; renderQueue();
  saveQueue(session);            // Q7：入队即落盘，刷新/关标签页不丢这条
  if (!s.draining) drain(session);
}
sendBtn.onclick = () => send();

// R2 回滚：物理截断到第 idx 条 user 消息之前（该问题及其后全部删除，memory 消息与
// trace 事件同源截断）。R15：区间内 agent 的文件改动随回滚一并撤销（host 冲突检测：
// 变更后又被人工改过的文件自动跳过，绝不硬覆盖）。resend=true 为「重新生成」。
async function rollbackTo(idx, resend, ut) {
  if (qState(session).running) { setStatus("对话进行中，请先停止或等待完成"); return; }
  const affected = [...fcSeen.values()].filter((e) => e.turn >= idx);
  const fileList = affected.slice(0, 8).map((e) => `${{ edit: "编辑", bash: "脚本", write: "写入" }[e.op] ?? e.op} ${e.path}`).join("\n");
  const undoNote = affected.length
    ? `\n\n同时撤销该轮起 agent 的文件改动（${affected.length} 个）：\n${fileList}${affected.length > 8 ? `\n…等共 ${affected.length} 个` : ""}\n（变更后又被手动修改的文件将自动跳过）`
    : "";
  if (!confirm(resend ? `回滚本问题并重新生成回答？${undoNote}` : `回滚到该问题之前？（该问题及其后的对话将被删除）${undoNote}`)) return;
  // D 轮：回滚前把输入框内容存成草稿——用户重打过的字不该因为点了回滚就没了。
  // X1：同时记下将被删掉的那条提问，回滚完成后预填进输入框（改一改即可再发）。
  // X1e：「重新生成」本来就会把原文发出去（下面 send(ut.text)），不能同时预填——
  // 否则输入框留着这句话、而它已经在跑，看起来像没发出去。只有普通回滚才预填。
  stashRollbackTarget(resend ? null : ut);
  stashDraftForRollback();
  try {
    const v = await api("POST", "/api/chat/rollback", { session_id: session, upto_user_index: idx });
    const un = v.undone?.length ?? 0, sk = v.skipped?.length ?? 0;
    var undoMsg = (un || sk) ? `已撤销 ${un} 个文件改动` + (sk ? `，${sk} 个跳过（文件已被修改）` : "") : null;
  } catch (e) { setStatus("回滚失败: " + e.message); return; }
  dropSessionCache(session); // 回滚会 connect(0)，缓存必须失效，防已删内容复活
  connect(0); // 全量重放截断后的历史（重置 SSE 游标）
  restoreDraftAfterRollback(); // D 轮：输入框/附件还回去（输入框已有新内容则不覆盖）
  if (undoMsg) setStatus(undoMsg);
  if (undoMsg) setStatus(undoMsg);
  if (resend && ut) send(ut.text, ut.atts || []);
}

// P2/T1 停止：发送期间可见。置位取消信号（agent-loop 在波次间/轮次边界收敛为 K499），
// 随即 abort 阻塞中的 /api/chat（UI 即时解锁，不再等服务端收敛）。
stopBtn.onclick = async () => {
  stopBtn.disabled = true;
  try {
    const r = await fetch(`/api/chat/cancel?session=${encodeURIComponent(session)}`, { method: "POST" });
    const v = await r.json();
    if (v.ok) {
      setStatus("正在停止…");
      // Q6：用户主动停止——被打断那条直接丢弃（不续跑）。标记须在 abort 之前落。
      markRunningStopped(session);
      returnQueueToInput();             // pi 语义：排队消息退回编辑器，不静默丢弃
      qState(session).abort?.abort();
    } else setStatus("停止失败: " + (v.error?.message ?? r.status));
  } catch (e) { setStatus("停止失败: " + e); }
  finally { stopBtn.disabled = false; }
};
input.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    if (e.altKey) interruptAndSend();  // Alt+Enter：硬插队（打断当前，本条先跑，原条回头续跑）
    else send();                        // Enter：排队（follow-up）
  } else if (e.key === "Escape" && qState(session).running) {
    e.preventDefault(); stopBtn.onclick(); // Esc：硬中断并退回队列
  }
});
// V15（I-20261115-04）：撤掉 composer 旁的独立「插队」键。
// 用户原话：「不需要单独在发送旁边显示插队按钮，就是有了新任务后它会自动排队，
// 然后在排队的哪里有插队的选项发送过去就好了。」
// 故插队入口**统一收回到待发队列行内 ⏭**（runQueueNow → 硬插队 → 被打断者回头续跑），
// composer 只保留「停止 / 发送」。Alt+Enter 的硬插队语义保留（同一套 interruptAndSend），
// 但不再有独立的可见按钮。
input.addEventListener("input", () => {
  input.style.height = "48px";
  input.style.height = Math.min(input.scrollHeight, 180) + "px";
});
input.focus();


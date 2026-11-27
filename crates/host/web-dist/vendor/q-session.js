"use strict";
// ── Q 轮：per-session 运行态 / 显式队列 / 会话视图缓存 ──────────────────────
// 旧实现把「忙」压在页面级 DOM（sendBtn.disabled）与单变量 chatAbort 上：A 会话
// 运行中切到 B，按钮仍禁用、abort 句柄串台，观感就是「新对话开不了任务」。
// 这里把运行态提升为 per-session 表；send/停止/插队全部按 sid 对位。队列只活在
// 当前标签页内存（服务端持久队列见 Phase 3），刷新/多标签页不共享，语义显式。
// 本文件先于 app-core/app-events 载入：只声明函数，所有跨文件全局在调用时才解析。
const runState = new Map();
// ── Q7 待发队列不丢（I-20261114-03）──────────────────────────────────────────
// 事故：插队或按「停止 / Esc」之后，待发队列整批消失。三个独立成因，逐条修：
//
// ① 队列只活在标签页内存（runState），刷新或重开标签页即归零。
//    → 落 sessionStorage，按会话分键（ra.queue#<sid>），进页面时还原。
//      只存文本与附件**计数**：附件 base64 动辄数 MB，塞 sessionStorage 会撞 5MB
//      配额并拖垮启动；还原条目标 restored，提示「原有 N 个附件需重新挂载」。
//
// ② returnQueueToInput() 用 `s.queue.length = draining ? 1 : 0` 就地**删除**待发项，
//    再把文字拼进输入框。于是用户接着打新指令时，send() 的 input.value = "" 把它们
//    一起清掉 —— 队列与草稿同时消失，这就是用户看到的「排队的全没了」。
//    → 改为原地标 halted：条目留在待发面板（可 ⏭ 重发 / × 删除 / ⧉ 复制），
//      不再往输入框里灌文字（避免「输入框一份 + 队列一份」被重复执行）。
//
// ③ 停止后若队列仍有条目，drain 的 `while (s.queue.length)` 会立刻续跑下一条，
//    等于「停止无效」。→ drain 遇 halted 队首即 break，等用户显式恢复。
//    恢复时机：用户点该条 ⏭，或直接发新消息（send 会把 halted 一并解除，保持原序）。
const QKEY = (sid) => "ra.queue#" + sid;
function saveQueue(sid) {
  try {
    const st = runState.get(sid);
    if (!st) return;
    // 执行中那条（draining 时 queue[0]）不存：它已经进了服务端 chat，刷新后由
    // trace 重放；存下来会让用户看到「同一条发两遍」。
    const rest = st.draining ? st.queue.slice(1) : st.queue.slice();
    const slim = rest.map((it) => ({ text: it.text, nAtts: (it.atts || []).length, halted: !!it.halted }));
    if (slim.length) sessionStorage.setItem(QKEY(sid), JSON.stringify(slim));
    else sessionStorage.removeItem(QKEY(sid));
  } catch { /* 配额满 / 隐私模式：退回纯内存，不阻断发送 */ }
}
function loadQueue(sid) {
  try {
    const raw = sessionStorage.getItem(QKEY(sid));
    if (!raw) return [];
    const arr = JSON.parse(raw);
    if (!Array.isArray(arr)) return [];
    // 一律以 halted 还原：刷新时不知道上一条执行到哪，静默自动续跑比「等用户确认」更危险。
    return arr
      .filter((it) => it && typeof it.text === "string" && it.text.trim())
      .map((it) => ({ text: it.text, atts: [], restored: true, lostAtts: it.nAtts || 0, halted: true }));
  } catch { return []; }
}
function qState(sid) {
  let s = runState.get(sid);
  if (!s) {
    s = { running: false, abort: null, queue: [], draining: false };
    s.queue = loadQueue(sid);          // Q7：刷新后把上次没跑的还回来（暂留态）
    runState.set(sid, s);
  }
  return s;
}

// DOM 快照缓存：离开会话时摘下 feed 的活节点 + 精确 cursor + 滚动位置 + 纯数据状态；
// 切回时把节点原样挂回瞬时显示，再用 EventSource(after=cursor) 只补增量。
const sessionCache = new Map();
const CACHE_MAX = 8;
let queueOpen = true;
let preciseCursor = false;
function dropSessionCache(sid) { sessionCache.delete(sid); }
function snapshotSessionFeed(sid) {
  if (typeof feed === "undefined" || !feed) return;
  sessionCache.delete(sid);
  // Q 轮修正：缓存的是「活的 DOM 节点」而不是 innerHTML 字符串。字符串快照经
  // 序列化再解析后，节点上挂的 expando（_fcpath/_fcargs/chip._items）与闭包
  // （chip.onclick / 工具卡事件）全部丢失 —— 缓存会话里「文件变更」chip 点不开、
  // 变更抽屉定位不到工具卡、⇄ diff 取不到 args。搬移节点引用即可原样保留。
  const nodes = [...feed.childNodes];
  sessionCache.set(sid, {
    nodes,
    cursor: lastAfter,
    scrollTop: ($("stream") || {}).scrollTop ?? 0,
    userTurns: userTurns.map((u) => ({ ...u, atts: (u.atts || []).slice() })),
    doneSids: [...doneSids],
    tail: turnTail ?? null,
    fcBox: turnFcBox ?? null,
    srcBox: turnSourceBox ?? null,
    artifacts: turnArtifacts.slice(),
    streamState: typeof snapshotStreamState === "function" ? snapshotStreamState() : null,
    fcSeen: typeof snapshotFileChanges === "function" ? snapshotFileChanges() : null,
  });
  while (sessionCache.size > CACHE_MAX) sessionCache.delete(sessionCache.keys().next().value);
}
function restoreSessionFeed(sid) {
  const c = sessionCache.get(sid);
  if (!c) return null;
  resetSessionFeed();                    // 集中重置（P7），随后覆盖纯数据状态
  feed.replaceChildren(...c.nodes);       // 搬移活节点：expando / 事件闭包原样保留
  lastAfter = c.cursor || 0;
  preciseCursor = lastAfter > 0;
  userTurns = c.userTurns || [];
  doneSids.clear();
  if (c.streamState && typeof restoreStreamState === "function") restoreStreamState(c.streamState);
  for (const s of c.doneSids || []) doneSids.add(s);
  if (c.fcSeen && typeof restoreFileChanges === "function") restoreFileChanges(c.fcSeen);
  // 回合尾/聚合 chip 的模块级指针一并接回，否则后续事件会另起一份重复卡。
  turnTail = c.tail ?? null;
  turnFcBox = c.fcBox ?? null;
  turnSourceBox = c.srcBox ?? null;
  turnArtifacts = c.artifacts || [];
    const box = $("stream");
  if (box) {
    // CS1：恢复会话后默认贴底到最新信息（用户不必每次手动下拉）。
    // 等 DOM 挂载/布局完成后再置底，避免被随后的增量重放覆盖。
    requestAnimationFrame(() => { box.scrollTop = box.scrollHeight; });
  }
  return c;
}

function syncRunUI() {
  const s = qState(session);
  // G3（I-20261108-05）：运行中**不再禁用**发送键。
  // 原话「有执行任务的时候，发送键点不了」，而 Enter 在同样时刻是照常入队的——
  // 两条路径语义分叉才是 bug：Enter 走 send() → queue.push + drain 单消费者，
  // 按钮被 disabled 只是把人挡在一个本来就安全的操作外面。
  // 运行中入队不会抢同会话闸（drain 在 finally 之后才发下一条），所以禁用没有
  // 保护任何东西，只制造「键盘能发、鼠标不能发」的不一致。
  // 改为视觉提示 + title 说明「会排队」：用户看得见行为的代价，而不是按钮变灰。
  sendBtn.disabled = false;
  sendBtn.classList.toggle("queued", s.running);
  sendBtn.title = s.running
    ? "当前任务执行中：点发送与 Enter 等效，会排进待发队列，本轮跑完自动开始"
    : "发送（Enter）";
  stopBtn.hidden = !s.running;
  renderQueue();
}
function renderQueue() {
  const box = $("queue-box"), list = $("queue-list"), cnt = $("queue-count");
  if (!box || !list) return;
  const st = qState(session), q = st.queue;
  box.hidden = !queueOpen || q.length === 0;
  if (cnt) cnt.textContent = "待发 " + q.length + " 条";
  list.innerHTML = "";
  q.forEach((item, i) => {
    // Q5：第 0 条即「正在执行」的那条（drain 驻留到它跑完）；其余才是真待发。
    const running = st.draining && i === 0;
    // Q7：被「停止」留下的待发项——原地保留，标 halted 可一键重发。
    if (item.halted && !running) {
      row.classList.add("halted");
      const tag = document.createElement("span");
      tag.className = "qrun"; tag.textContent = "‖ 已暂留";
      tag.title = "点 ⏭ 重新执行这条；点 × 丢掉";
      row.insertBefore(tag, row.querySelector(".qexec"));
    }
    // Q7：刷新后从 sessionStorage 还原的条目——附件没跟着回来，显式提示。
    if (item.restored && !running) {
      row.classList.add("restored");
      const tag = document.createElement("span");
      tag.className = "qrun"; tag.textContent = "↺ 已还原";
      tag.title = item.lostAtts
        ? `页面刷新后还原；原有 ${item.lostAtts} 个附件需重新挂载`
        : "页面刷新后从本地存储还原";
      row.insertBefore(tag, row.querySelector(".qexec"));
    }
    const row = el(`<div class="qitem"><span class="qtext"></span><button class="qexec">⏭</button><button class="qup">↑</button><button class="qedit">✎</button><button class="qcp">⧉</button><button class="qrm">×</button></div>`);
    if (running) {
      row.classList.add("running");
      const tag = document.createElement("span");
      tag.className = "qrun"; tag.textContent = "执行中";
      row.insertBefore(tag, row.querySelector(".qexec"));
    }
    // Q6：被插队打断、已排队等待「回头续跑」的那条——显式标注，让用户看见它没丢。
    if (item.pendingResume && !running) {
      row.classList.add("resume");
      const tag = document.createElement("span");
      tag.className = "qrun"; tag.textContent = "⟲ 待续跑";
      tag.title = "被插队打断，插队那条跑完自动回头继续这条";
      row.insertBefore(tag, row.querySelector(".qexec"));
    }
    // K1：断线导致没发出去的那条——显式标注「等重发」，避免用户以为它已经跑过了。
    if (item.offline && !running) {
      row.classList.add("offline");
      const tag = document.createElement("span");
      tag.className = "qrun"; tag.textContent = "⇄ 待重发";
      tag.title = "连接断开，这条没送到服务端；点 ⏭ 立即重发，或直接再点发送";
      row.insertBefore(tag, row.querySelector(".qexec"));
    }
    row.querySelector(".qtext").textContent = item.text;
    const [exec, up, edit, cp, rm] = row.querySelectorAll("button");
    exec.title = running ? "正在执行" : "立即执行这条（打断当前任务，优先跑它）";
    up.title = "上移（更早执行）";
    edit.title = "编辑这条（改完就地生效；已执行中那条不能改）";
    cp.title = "复制到输入框（内容太多时就地改不如下去改）";
    rm.title = "从待发队列删除";
    exec.disabled = running; up.disabled = running || (st.draining && i === 1); rm.disabled = running;
    // D 轮：编辑只对「待发」开放。执行中那条的文本已经进了 chat，改本地串不改服务端事实，
    // 只会让用户以为改生效了——所以按钮直接禁用而不是「点了没反应」。
    edit.disabled = running;
    exec.onclick = () => { runQueueNow(i); };
    // 执行中那条（index 0）不可被上移顶掉；故 index 1 的上移在 draining 期间禁用。
    up.onclick = () => { if (i > 0 && !(st.draining && i === 1)) { [q[i - 1], q[i]] = [q[i], q[i - 1]]; renderQueue(); } };
    edit.onclick = () => startQueueEdit(row, item, running);
    cp.onclick = () => copyQueueItem(item);
    rm.onclick = () => { q.splice(i, 1); renderQueue(); };
    list.appendChild(row);
  });
}

// D 轮：队列行就地编辑。I-20261107-02 原话「执行排队（待发指令）也可以支持编辑或者
// 复制？否则打上去就只能删除了」——原实现只有 × 删除，删了就得整段重打。
// 就地换 input 而不是弹框：一行指令的编辑半径就在这一行。
function startQueueEdit(row, item, running) {
  if (running) return;
  const textEl = row.querySelector(".qtext");
  if (!textEl || textEl.dataset.editing === "1") return;
  textEl.dataset.editing = "1";
  const input = document.createElement("input");
  input.className = "qedit-input";
  input.value = item.text;
  textEl.replaceWith(input);
  input.focus();
  input.setSelectionRange(input.value.length, input.value.length);
  let done = false;
  const finish = (save) => {
    if (done) return;
    done = true;
    const next = input.value.trim();
    if (save && next && next !== item.text) {
      item.text = next;
      setStatus("已更新待发指令");
    }
    renderQueue();
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); finish(true); }
    else if (e.key === "Escape") { e.preventDefault(); finish(false); }
  });
  input.addEventListener("blur", () => finish(true));
}

// D 轮：复制一条到输入框。为什么不直接 copy 到剪贴板：用户要的通常是「拿这条当基础
// 改一版」，落进输入框才改得动；剪贴板还要切窗口粘贴。
function copyQueueItem(item) {
  const cur = input.value;
  const sep = cur && !/\s$/.test(cur) ? "\n" : "";
  input.value = cur + sep + item.text;
  input.dispatchEvent(new Event("input"));
  input.focus();
  queueOpen = true;
  renderQueue();
  setStatus("已复制到输入框");
}

// D 轮：回滚保留草稿（I-20261107-02 原话「回滚的时候自动保留之前的文字，否则都要
// 再重新打一遍」）。回滚会 connect(0) 清空 feed，但**输入框与待发队列是用户的东西**，
// 不该被"清场"牵连：这里在回滚前把当前输入框内容存成草稿，回滚后若输入框还空着就还回去。
// 不存 localStorage——草稿的生命周期就该是这一次操作。
let rollbackDraft = null;

// R2 补强（X1）：回滚到某条提问之前，就把那条提问本身预填进输入框。
// D 轮只还了「用户当时正在打但还没发」的草稿；而回滚的常见意图是「这条问得不对，
// 改一改再问一次」——被回滚掉的那条提问恰恰是最该预填的文字。原实现里它只在
// 「重新生成」（resend=true 复用 ut.text）路径才有，普通回滚按钮走的是
// `rollbackTo(idx, false)`，索引传了、ut 没传，用户只好手打一遍。
// pendingRollbackText 与 rollbackDraft 分开存：一个是「回滚掉了什么」，一个是
// 「回滚前正在打什么」。两者都有时优先还提问原文——那一轮已经删掉了，草稿还在
// 输入框里没丢。附件同理（trace user 事件带全量 b64）。
let pendingRollbackText = null;

function stashDraftForRollback() {
  rollbackDraft = { text: input.value, atts: pendingAtts.slice() };
  // 若输入框非空（用户已在准备下一句），不覆盖它——那是他手打的东西。
  if (rollbackDraft.text.trim()) pendingRollbackText = null;
}

// 由回滚入口在调 stashDraftForRollback() 之前调用，记下将被删掉的那条提问。
function stashRollbackTarget(turn) {
  if (!turn) return;
  pendingRollbackText = { text: turn.text ?? "", atts: (turn.atts || []).slice() };
}

function restoreDraftAfterRollback() {
  const target = pendingRollbackText;
  pendingRollbackText = null;
  const d = rollbackDraft;
  rollbackDraft = null;
  // X1：优先级 = 被回滚的提问原文 > 回滚前的未发草稿。两者都只在输入框仍空时写入，
  // 用户回滚后已经开始打新字就绝不覆盖。
  const pick = (target && target.text) ? target : (d && d.text) ? d : null;
  if (pick && !input.value.trim()) {
    input.value = pick.text;
    input.style.height = "48px";
    input.style.height = Math.min(input.scrollHeight, 180) + "px";
  }
  const atts = (target && target.atts?.length) ? target.atts : (d && d.atts) || [];
  if (atts.length && !pendingAtts.length) pendingAtts = atts.slice();
  renderAtts();
  // 光标落到末尾：预填的目的就是「改一改再发」，不是让人先按 End。
  if (pick && input.value === pick.text) {
    input.focus();
    try { input.setSelectionRange(input.value.length, input.value.length); } catch { /* 非文本框忽略 */ }
  }
}
async function postChatDirect(sid, item) {
  const s = qState(sid);
  // 显式队列内部消化 K409：cancel/abort 后服务端仍要跑到轮次边界才释放同会话闸，
  // 直接发下一条会撞 409。K409 在 memory.append 之前返回，重试不会重复落 user 事件。
  // Q6：返回结局 "ok" | "error" | "aborted"——drain 据此区分「正常完成出队」与
  // 「被插队打断 → 挂起续跑」，不再依赖「await 一返回就出队」这条隐式约定。
  for (let attempt = 0; attempt < 24; attempt++) {
    // Q6：本条已被插队打断 / 被用户停止 → 立即让位，不再空转 K409 重试窗口
    // （最长 24×250ms），否则续跑要白等 6 秒。
    if (item.interrupted || item.stopped) return "aborted";
    s.abort = new AbortController();
    try {
      const r = await fetch("/api/chat", {
        method: "POST", headers: { "content-type": "application/json" }, signal: s.abort.signal,
        // Q6：pendingResume = 被插队打断后回头续跑。带 resume:true 让服务端跳过
        // 重复落库/重放 user 消息（否则前端多一个气泡、memory 多一条 user 轮次）。
        body: JSON.stringify({ session_id: sid, message: item.text, ...(item.atts && item.atts.length ? { attachments: item.atts } : {}), ...(item.pendingResume ? { resume: true } : {}) }),
      });
      const v = await r.json();
      if (v && v.error && v.error.code === "K409" && attempt < 23) {
        await new Promise((done) => setTimeout(done, 250));
        continue;
      }
      if (session === sid) setStatus(v.ok ? "" : "失败: " + (v.error?.message ?? r.status));
      return v.ok ? "ok" : "error";
    } catch (e) {
      if (e?.name === "AbortError") {
        // 打断了不要谎报「已停止」：结局由 drain 按 item 上的标记分流
        if (session === sid) setStatus(item.stopped ? "已停止（后台正在收敛）" : "已打断，稍后回到本条继续");
        return "aborted";
      }
      if (e?.name === "AbortError") {
        // 打断了不要谎报「已停止」：结局由 drain 按 item 上的标记分流
        if (session === sid) setStatus(item.stopped ? "已停止（后台正在收敛）" : "已打断，稍后回到本条继续");
        return "aborted";
      }
      // K1：连接层失败（host 重启 / 断网 / fetch reject）≠ 执行失败。
      // 旧实现在这里统一 return "error"，drain 见到非 aborted 就出队，于是断线瞬间
      // 整个待发队列被逐条清空——「排队指令全消失」。区分出来交给 drain 保留。
      if (session === sid) setStatus("请求失败: " + e);
      return "offline";
    } finally { s.abort = null; }
  }
  // K1：重试窗口耗尽仍拿不到服务端结论（多半是连接层问题）按 offline 处理，同样不吞条目。
  return "offline";
}
// Q6：续跑上限——防「插队↔被插队」极端来回 ping-pong 无界重放（超限即放弃并提示）。
const MAX_RESUME = 8;
function markRunningStopped(sid) {
  const s = qState(sid);
  // Q6：用户主动停止（停止键 / Esc）→ 被打断的那条不再续跑，直接丢弃。
  if (s.draining && s.queue[0]) s.queue[0].stopped = true;
}
function markRunningInterrupted(sid) {
  const s = qState(sid);
  // Q6：插队（⏭ / Alt+Enter）→ 被打断的那条挂起，等插队条跑完自动续跑。
  // 此刻被打断的仍在 index 0（插队条已排到它后面），故只标队首。
  if (s.draining && s.queue[0]) s.queue[0].interrupted = true;
}
async function drain(sid) {
  const s = qState(sid);
  if (s.draining) return;
  s.draining = true; s.running = true; syncRunUI();
  try {
    while (s.queue.length) {
      const item = s.queue[0];
      // Q7：队首是「停止时暂留」的条目 → 不再自动续跑，等用户显式恢复
      // （点该条 ⏭，或直接发新消息解除暂留）。否则停止之后队列会立刻接着跑下一条，
      // 用户看到的就是「按了停止，任务还在继续」，等于停止无效。
      if (item.halted) break;
      // Q5：队列本地驻留——第 0 条即「执行中」。postChatDirect 的 Promise 覆盖整段执行，
      // 跑完才出队；待发面板因此在整轮执行期间都可见，行内 ⏭/↑/× 才有操作窗口。
      item.pendingResume = false;         // Q6：轮到它了，不再是「待续跑」
      item.offline = false;               // K1：开始重试就撤掉「待重发」标注
      const outcome = await postChatDirect(sid, item);
      // K1（断线不吞队列）：outcome==="error" 分两种，不能一律当「已跑完」出队——
      //   · 服务端明确回了非 ok（如 K400/K500）：这条确实跑过了，出队合理；
      //   · 连接层失败（fetch reject：host 重启/断网）：「请求没送到」不等于「执行完」。
      // 旧实现一律出队，断线时 while 循环会把整个待发队列逐条清空（用户看到「排队的
      // 指令全没了」），且 drain 的 finally 把 running 置回 false，界面显示空闲。
      // 这里把「连接层失败」单独返回 offline：条目留在队首（下面 splice 跳过），
      // 循环立刻停下，等用户手动重发/重连后再 drain。
      if (outcome === "offline") {
        item.offline = true;              // 行内标注，让用户看见「这条还在这儿」
        renderQueue();
        if (session === sid) setStatus("连接断开：" + (item.text.slice(0, 24)) + " 等 " + s.queue.length + " 条仍留在待发队列");
        break;
      }
      const at = s.queue.indexOf(item);   // 执行期间可能被删除/上移：按身份删除，不盲目 shift
      if (at >= 0) s.queue.splice(at, 1);
      // Q6：结局分流——「插队打断」与「用户停止」走同一条 cancel+abort，但语义相反：
      //   · 插队（item.interrupted）：本条没做完，挂起，等插队条跑完自动回头续跑；
      //   · 停止（item.stopped）：用户就是要它别做了，直接丢弃。
      // 修复前两者都被无条件出队 → 插队一次，原任务从队列里永久消失（用户看到「回不去了」）。
      if (outcome === "aborted" && item.interrupted && !item.stopped) {
        item.interrupted = false;
        item.resumes = (item.resumes || 0) + 1;
        if (item.resumes > MAX_RESUME) {
          if (session === sid) setStatus("多次被插队中断，已放弃续跑：" + item.text.slice(0, 24));
        } else {
          // 插到刚打断它的那条（现 index 0）之后：插队条跑完出队，本条即回到队首续跑，
          // 且仍优先于插队条之后的所有待发项——「做完插队的活就回头」。
          item.pendingResume = true;
          s.queue.splice(Math.min(1, s.queue.length), 0, item);
        }
      }
      renderQueue();
    }
  } finally {
    s.draining = false; s.running = false; s.abort = null;
    saveQueue(sid);                      // Q7：出 drain 落盘，待发项跨刷新存活
    if (session === sid) { syncRunUI(); setStatus(""); input.focus(); }
  }
}
async function interruptAndSend() {
  const text = input.value.trim();
  if (!text) return;
  const atts = pendingAtts.splice(0);
  input.value = ""; input.style.height = "48px"; renderAtts();
  // V14（I-20261115-03）：**Alt+Enter = 硬插队**，不再是「轮次边界软注入」。
  //
  // 用户原话：「点了之后立即执行插队任务，然后正在执行的，默认进入排队，等插队的
  // 完成后继续按顺序进行排队任务」——这正是下面这套「cancel + abort + 挂起续跑」
  // 的语义，也**早就在代码里**（Q6，commit f2a683a）：打断当前 → 插队条插到它前面
  // → 插队条跑完出队 → 被打断那条回到队首续跑。
  //
  // 但 V13-Q（Q 轮 Phase 3）把 Alt+Enter 改道到服务端 steer：POST /api/queue
  // target=steer，服务端把文本放进 steer 缓冲，chat_run **下一轮开头**注入工作集
  // （chat.rs:629-645）。steer 的语义是「追加一条」，**不取消当前任务**——于是用户
  // 按了 Alt+Enter，看到的是「当前任务继续跑，我的话排在后面」。更糟的是它
  // **成功后直接 return**，把后面那套硬插队逻辑整段跳过了。
  //
  // 两个键位语义打架的代价是「插队键不插队」：用户以为在抢占，实际在排队。
  // 故此处删去 steer 分支，Alt+Enter 一律走硬插队路径。
  //
  // steer 的服务端能力**保留**（queue.rs:67-87 + chat.rs:629-645），供将来
  // 「不打断当前、只想追加一句」的场景显式调用；只是不再由 Alt+Enter 触发。
  const s = qState(session);
  // Q5：队列驻留后 index 0 是「执行中」，新条插到它之后，否则会把执行中的行顶成待发。
  s.queue.splice(s.draining ? 1 : 0, 0, { text, atts });
  queueOpen = true; renderQueue();
  // Q6：Alt+Enter 同样是「插队打断」——被打断那条要回头续跑，不能当用户停止丢掉。
  markRunningInterrupted(session);
  try {
    await fetch(`/api/chat/cancel?session=${encodeURIComponent(session)}`, { method: "POST" });
  } catch { /* 取消失败仍由 drain 的 K409 重试兜底 */ }
  s.abort?.abort();                           // 只断客户端 fetch；服务端到轮次边界收敛 K499
  if (!s.draining) drain(session);
}
async function runQueueNow(i) {
  const s = qState(session);
  const item = s.queue[i];
  if (!item) return;
  // Q5：队列驻留后第 0 条是「执行中」，目标是让它插到执行中那条之后（index 1），
  // 再打断当前任务——drain 的 await 一返回就出队第 0 条，随即轮到它。
  // Q7：用户点 ⏭ = 明确要「现在跑这条」，同时解除整个队列的暂留标记
  // （否则条目虽被移到队首，drain 仍会在 halted 处 break，点了没反应）。
  s.queue.forEach((it) => { it.halted = false; });
  s.queue.splice(i, 1);
  const at = s.draining ? 1 : 0;
  s.queue.splice(at, 0, item);
  queueOpen = true; renderQueue();
  if (!s.draining) { drain(session); return; }
  // 有在途任务：取消服务端回合 + 断客户端 fetch，让 drain 尽快轮到它。
  // Q6：先标记被打断的是「插队」而非「用户停止」——drain 据此把原任务挂起续跑，
  // 而不是像停止那样直接丢弃。（顺序要紧：必须在 abort 之前落标记。）
  markRunningInterrupted(session);
  try {
    await fetch(`/api/chat/cancel?session=${encodeURIComponent(session)}`, { method: "POST" });
  } catch { /* 取消失败由 drain 的 K409 重试兜底 */ }
  s.abort?.abort();
}
function returnQueueToInput() {
  const s = qState(session);
  // Q7（I-20261114-03）：原实现把待发项 `s.queue.length = draining ? 1 : 0` 就地
  // **销毁**，再把文字拼进输入框。两处致命：
  //   ① 用户接着打新指令时 send() 里 input.value = "" 把这批文字一起清掉；
  //   ② 待发面板立刻清空，用户看到的就是「排队的全没了」（事实也确实没了）。
  // 改为：条目原地保留并标 halted——面板上看得见、可 ⏭ 重发 / × 删除 / ⧉ 复制；
  // 不往输入框灌文字（否则「输入框一份 + 队列一份」会被重复执行）。
  // 点「停止」的语义随之变成「停下当前，队列暂留等我决定」，而不是「顺带清队」。
  const rest = s.draining ? s.queue.slice(1) : s.queue.slice();
  rest.forEach((p) => { p.halted = true; });
  saveQueue(session);
  renderAtts(); renderQueue();
}
function bindQueueUI() {
  const toggle = $("queue-toggle");
  if (toggle) toggle.onclick = () => {
    queueOpen = !queueOpen;
    toggle.textContent = queueOpen ? "隐藏" : "展开";
    renderQueue();
  };
  // Q5：composer 不再有「插队」键（恢复为纯「停止」），Alt+Enter 仍走 interruptAndSend。
}

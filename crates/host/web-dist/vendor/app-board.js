"use strict";
// ── 左侧看板：布局驱动的章节装配（C 轮）──
// 设计要点（docs/self-evolution/02-评价体系与看板插件化.md）：
// 1. **不自由排布**：章节来自 `.rsi/board.json` 的 `layout.sections`，但 kind 只剩三类
//    （tasks / metrics / inspirations）、分组维度三种、筛选三种、指标四种，且**渲染顺序
//    由 kind 的固定优先级钉死**（任务 → 指标 → 灵感）。配置写乱序也只影响同 kind 内次序。
//    「插件化」= 加章节不用改前端；「有约束」= 能加的东西是封闭枚举，不是任意 DOM。
// 2. **只读任务 + 可写灵感**：任务由 agent 建与推进（用户不管背后的项目分类），
//    用户在 UI 只能记灵感、归档灵感。这是权限的界面表达，不是功能缺失。
// 3. **轮询而非推送**：看板变化频率是「分钟级」，SSE 加一路事件流得不偿失；
//    页面可见时每 20s 拉一次 + 操作后立即拉，足够。
const BOARD_ORDER = ["planning", "doing", "verifying", "done", "blocked", "cancelled"];
const BOARD_LABEL = {
  planning: "计划", doing: "进行", verifying: "验收", done: "完成", blocked: "阻塞", cancelled: "取消",
};
const PRIO_RANK = { P0: 0, P1: 1, P2: 2, P3: 3 };
const INSP_LABEL = { open: "待议", adopted: "已采纳", archived: "已归档" };
const VERDICT_LABEL = { none: "未跑", pending: "待人判", pass: "通过", fail: "不通过" };
// 章节种类与顺序（与 scripts/board.py::validate_layout 的 rank 逐字同源）。
const KIND_ORDER = ["tasks", "metrics", "inspirations"];
const KIND_LABEL = { tasks: "任务", metrics: "指标", inspirations: "灵感" };
// 无 layout 时的兜底（旧板 / 手改坏板也能渲染，不白屏）。
const DEFAULT_SECTIONS = [
  { id: "tasks", kind: "tasks", title: "任务看板", group_by: "status", filter: "all", limit: 30 },
  { id: "insp", kind: "inspirations", title: "灵感" },
];

let boardData = null;       // 最近一次 GET /api/board 的 payload
let boardFilter = "all";    // all | open | done（只作用于 tasks 章节）
// v12（B 方案）：视图档位。`all` = 这本账的全部；`mine` = 只显示本窗口所属工作区
// 产生的条目。
//
// 为什么是「视图开关」而不是「每个工作区一块板」：账本只有一本（`.rsi/board.json`，
// 锚代码根），工作区只是**归属标签**。拆板会退回「每个窗口各一块、互不相见」——
// 那正是用户报过的 bug（新窗口里任务在跑、左侧看板没变化）。
//
// 默认 `all` 而不是 `mine`：用户打开看板最常见的意图是「我现在总共还有多少事」，
// 而不是「这个项目还有多少事」。且老条目没有归属标签，默认 `mine` 会让它们
// 看起来像消失了——那是让人以为丢账的观感，比多显示几条更糟。
let boardScope = "all";     // all | mine
// 记在 localStorage：看板是长期视图，不该每次打开都要重选一遍档位。
const SCOPE_KEY = "ra.boardscope";
try {
  const v = localStorage.getItem(SCOPE_KEY);
  if (v === "mine" || v === "all") boardScope = v;
} catch { /* 隐私模式/坏值：用缺省 all，不阻断渲染 */ }
let boardTimer = null;
const boardOpen = new Set(); // 展开中的任务行 id（轮询重绘不丢展开态）

function boardEsc(s) { return esc(String(s ?? "")); }

function boardReq(method, url, body) {
  return api(method, url, body);
}

function boardSections() {
  const raw = boardData?.board?.layout?.sections;
  const secs = Array.isArray(raw) && raw.length ? raw : DEFAULT_SECTIONS;
  // 前端二次校验：白名单外 kind 直接丢弃（后端已校验，这里防手改文件 + 旧 host）
  const kept = secs.filter((s) => KIND_ORDER.includes(s?.kind) && typeof s?.id === "string");
  const rank = (k) => { const i = KIND_ORDER.indexOf(k); return i < 0 ? 99 : i; };
  return kept
    .map((s, i) => ({ s, i }))
    .sort((a, b) => (rank(a.s.kind) - rank(b.s.kind)) || (a.i - b.i))
    .map((x) => x.s);
}

// v13：一条任务/灵感属于哪个工作区（显示用标签）。空标签 = 归属未知（老条目），
// **不编一个「未知」写出来**——那会被读成「丢失」。返回空串，调用方据此不渲染。
function wsTagOf(it) {
  return String(it?.workspace ?? "").trim();
}

// 任务分组：状态优先，其次优先级，再次更新时间倒序（新改动的在同组更靠上）。
function groupTasks(tasks, groupBy) {
  const key = groupBy === "priority" ? (t) => (PRIO_RANK[t.priority] ?? 9) : null;
  if (key) {
    const buckets = new Map();
    for (const t of tasks) {
      const k = t.priority ?? "P2";
      if (!buckets.has(k)) buckets.set(k, []);
      buckets.get(k).push(t);
    }
    const order = ["P0", "P1", "P2", "P3"].filter((p) => buckets.has(p))
      .concat([...buckets.keys()].filter((p) => !["P0", "P1", "P2", "P3"].includes(p)));
    const out = new Map();
    for (const p of order) out.set(p, sortRows(buckets.get(p)));
    return out;
  }
  if (groupBy === "skill") {
    const out = new Map();
    for (const t of tasks) {
      const skills = (t.resourcing?.skills ?? []);
      const keys = skills.length ? skills : ["（未指派）"];
      for (const s of keys) {
        if (!out.has(s)) out.set(s, []);
        out.get(s).push(t);
      }
    }
    for (const s of out.keys()) out.set(s, sortRows(out.get(s)));
    return out;
  }
  // v13：按工作区分组。**本工作区排最前**——用户开这个窗口就是来看自己项目的活，
  // 让他先滚过 28 条别的项目的任务才能看到自己那条，等于没做这个功能。
  // 其余按条数降序（活多的项目更可能是他正在想的那个），同条数按标签字典序（稳定，
  // 不会因为轮询重排而跳动）。空标签排最后：归属未知不该抢在已知之上。
  if (groupBy === "workspace") {
    const out = new Map();
    for (const t of tasks) {
      const k = wsTagOf(t);
      if (!out.has(k)) out.set(k, []);
      out.get(k).push(t);
    }
    for (const k of out.keys()) out.set(k, sortRows(out.get(k)));
    const mine = boardMine();
    const keys = [...out.keys()].sort((a, b) => {
      const am = a === mine && a ? 0 : 1, bm = b === mine && b ? 0 : 1;
      if (am !== bm) return am - bm;
      const ae = a ? 0 : 1, be = b ? 0 : 1;
      if (ae !== be) return ae - be;
      const d = out.get(b).length - out.get(a).length;
      return d !== 0 ? d : a.localeCompare(b);
    });
    return new Map(keys.map((k) => [k, out.get(k)]));
  }
  // v13：按工作区分组。**本工作区排最前**——用户开这个窗口就是来看自己项目的活，
  // 让他先滚过 28 条别的项目的任务才能看到自己那条，等于没做这个功能。
  // 其余按条数降序（活多的项目更可能是他正在想的那个），同条数按标签字典序（稳定，
  // 不会因为轮询重排而跳动）。空标签排最后：归属未知不该抢在已知之上。
  if (groupBy === "workspace") {
    const out = new Map();
    for (const t of tasks) {
      const k = wsTagOf(t);
      if (!out.has(k)) out.set(k, []);
      out.get(k).push(t);
    }
    for (const k of out.keys()) out.set(k, sortRows(out.get(k)));
    const mine = boardMine();
    const keys = [...out.keys()].sort((a, b) => {
      const am = a === mine && a ? 0 : 1, bm = b === mine && b ? 0 : 1;
      if (am !== bm) return am - bm;
      const ae = a ? 0 : 1, be = b ? 0 : 1;
      if (ae !== be) return ae - be;
      const d = out.get(b).length - out.get(a).length;
      return d !== 0 ? d : a.localeCompare(b);
    });
    return new Map(keys.map((k) => [k, out.get(k)]));
  }
  const by = new Map(BOARD_ORDER.map((s) => [s, []]));
  for (const t of tasks) {
    const st = by.has(t.status) ? t.status : "planning";
    by.get(st).push(t);
  }
  for (const arr of by.values()) sortRows(arr);
  return by;
}

function sortRows(arr) {
  // v13：**同组内本工作区的行排最前**。理由：用户开这个窗口就是来看自己项目的活，
  // 而「完成」组里有 28 条别的项目的任务、他刚做完的那条排在中间，等于还是没看见。
  // 优先级/时间仍是主序，归属只是最外层的一刀——换项目看时同一套规则照样成立。
  const mine = boardMine();
  const mineRank = (t) => (mine && wsTagOf(t) === mine ? 0 : 1);
  arr.sort((a, b) => {
    const ma = mineRank(a), mb = mineRank(b);
    if (ma !== mb) return ma - mb;
    const pa = PRIO_RANK[a.priority] ?? 9, pb = PRIO_RANK[b.priority] ?? 9;
    if (pa !== pb) return pa - pb;
    return String(b.updated_at ?? "").localeCompare(String(a.updated_at ?? ""));
  });
  return arr;
}

function taskVisible(t, filter) {
  const f = filter ?? boardFilter;
  if (f === "open") return t.status !== "done" && t.status !== "cancelled";
  if (f === "done") return t.status === "done" || t.status === "cancelled";
  return true;
}

// v12：当前窗口的工作区标签（后端 GET /api/board 回显）。解析不出就是空串。
function boardMine() {
  return String(boardData?.workspace ?? "");
}

// v12：归属过滤。**空标签的条目在任何档位都显示** —— 空是「归属未知」（老条目），
// 不是「属于别的项目」。按等值硬过滤会让既有条目在「本项目」档里集体消失，
// 用户会以为账被删了一半；宁可多显示几条，也不让人怀疑数据没了。
function scopeVisible(it) {
  if (boardScope !== "mine") return true;
  const tag = String(it?.workspace ?? "");
  return !tag || tag === boardMine();
}

// v12：档位开关条（tasks 与 inspirations 共用同一档位）。
//
// 位置取舍：放在每个章节里，而不是页面顶部一个全局开关。看板可以有多块同类章节
// （用户能加章节），全局开关会让「这一个现在是全部还是本项目」变得不可知——
// 控件贴着它控制的东西放，是唯一不会被误读的做法。
function scopeBar() {
  const bar = el("div", "sec-filter scope");
  const mine = boardMine();
  for (const [v, label] of [["all", "全部"], ["mine", mine ? `本项目·${mine}` : "本项目"]]) {
    const b = el("button", v === boardScope ? "on" : "");
    b.dataset.scope = v;
    b.textContent = label;
    b.title = v === "mine"
      ? "只看本窗口所属工作区产生的条目（归属未知的老条目仍会显示）"
      : "整本账：所有工作区的任务与灵感";
    // 没有工作区标签时「本项目」无从判定——点它等于把全部藏起来，故禁用而不是骗人。
    if (v === "mine" && !mine) {
      b.disabled = true;
      b.title = "当前窗口没有工作区标签（未设置 WORKSPACE_ROOT），无法按项目过滤";
    }
    b.onclick = () => {
      boardScope = v;
      try { localStorage.setItem(SCOPE_KEY, v); } catch { /* 存不下就只在本次生效 */ }
      refreshSections();
      if (typeof renderBoardWin === "function") renderBoardWin();
    };
    bar.appendChild(b);
  }
  return bar;
}

// FE9：与 app-core.js 的 el(html) 消歧。本文件最后加载，原来这 5 行把全局 el 静默
// 改写成了 tag 版——app-events/stream/files 里 el('<div …>') 全抛 InvalidCharacterError，
// 用户气泡、工具卡、流式回答一个都不渲染（观感就是「发消息没有回应」，刷新后历史也空）。
// 两种调用形态可判别：HTML 串必以 '<' 开头，标签名不会。故合并为一个分发函数：
// 既不改动本文件 45 处 el(tag, cls, html) 调用点，也不再让后加载者改写先加载者的语义。
function el(tag, cls, html) {
  if (typeof tag === "string" && tag.trimStart().startsWith("<")) {
    const box = document.createElement("div");
    box.innerHTML = tag;
    return box.firstElementChild;
  }
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (html != null) n.innerHTML = html;
  return n;
}

function sectionHead(sec, countText, withRefresh) {
  const head = el("div", "sec-head");
  const title = el("span", "sec-title");
  title.textContent = sec.title || KIND_LABEL[sec.kind] || sec.id;
  const count = el("span", "sec-count");
  count.textContent = countText ?? "";
  head.append(title, count);
  if (withRefresh) {
    const btn = el("button", "sec-btn");
    btn.title = "重新读取看板（.rsi/board.json）";
    btn.textContent = "⟳";
    btn.onclick = () => refreshBoard();
    head.appendChild(btn);
  }
  return head;
}

// V14（I-20261115-03）：状态筛选条抽成共用函数（侧栏 + 看板弹窗两处都用）。
//
// 为什么抽出来：**原来的筛选条是纯装饰**。侧栏 renderTasksSection 渲染它、
// 点它也会改 boardFilter 高亮，但过滤那一步传的是 `sec.filter`（章节配置，
// 本来就是 "all"）而不是用户点的 boardFilter——于是「点未完成 → 按钮变亮 →
// 列表一条没少」。用户原话：「全部、已完成、未完成的标签相当于是没用的」。
//
// 抽成函数是为了让弹窗也能挂同一套——弹窗是「摊开细看」的宽面，用户在这里
// 最可能问「哪些还没做完」，两处各写一份迟早又分叉。
function filterBar() {
  const bar = el("div", "sec-filter");
  for (const [f, label] of [["all", "全部"], ["open", "未完成"], ["done", "已完成"]]) {
    const b = el("button", f === boardFilter ? "on" : "");
    b.dataset.f = f;
    b.textContent = label;
    b.title = f === "all" ? "全部任务"
      : f === "open" ? "未完成（计划 / 进行 / 验收 / 阻塞）"
      : "已落定（完成 / 取消）";
    b.onclick = () => {
      boardFilter = f;
      refreshSections();          // 侧栏重建
      renderBoardWin();           // 弹窗同步（同一份 boardFilter）
    };
    bar.appendChild(b);
  }
  return bar;
}

// V14（I-20261115-03）：侧栏任务章节的过滤改用用户点的 boardFilter。
//
// 原代码传的是 `sec.filter`——那是**章节配置**里的静态值（布局里写死 "all"），
// 与用户点的按钮无关。按钮只切高亮，数据不动，这就是「标签相当于是没用的」。
// 章节自己指定 filter=open/done 时仍以章节为准（那是有意的预筛）。
function taskVisibleInSection(t, sec) {
  if (!sec.filter || sec.filter === "all") return taskVisible(t, boardFilter ?? "all");
  return taskVisible(t, sec.filter);
}

// ── tasks 章节 ───────────────────────────────────────────────────────────────
function renderTasksSection(sec) {
  const wrap = el("section", "board-sec");
  wrap.dataset.kind = "tasks";
  wrap.dataset.sec = sec.id;
  const tasksAll = (boardData?.board?.tasks ?? [])
    .filter((t) => taskVisibleInSection(t, sec))
    .filter(scopeVisible);
  const sum = boardData?.summary?.tasks;
  const bs = sum?.by_status ?? {};
  const openN = (bs.planning ?? 0) + (bs.doing ?? 0) + (bs.verifying ?? 0);
  wrap.appendChild(sectionHead(sec, `${openN} 进行中 / ${sum?.total ?? 0}`, true));

  // v12：档位开关。放在状态筛选条之前——它决定「哪些条目进入这一章」，
  // 状态筛选决定「进去之后怎么切」，顺序与语义一致。
  wrap.appendChild(scopeBar());

  // 筛选只在「该章节不限 filter」时提供（章节自己指定 filter=open 时不再给开关）
  const showFilter = !sec.filter || sec.filter === "all";
  if (showFilter) wrap.appendChild(filterBar());

  const body = el("div", "sec-body");
  body.id = `sec-${sec.id}-body`;
  wrap.appendChild(body);
  const limit = Number(sec.limit ?? 30);
  if (!tasksAll.length) {
    body.appendChild(el("div", "empty",
      boardScope === "mine" && boardMine()
        ? `本项目（${boardMine()}）下暂无任务；切「全部」看整本账`
        : (boardFilter === "all" ? "暂无任务（agent 会把成规模的事登记到这里）" : "该筛选下没有任务")));
    return wrap;
  }
  // F1：分组折叠——活跃组默认展开，落定组（完成/取消）默认折成一行。
  // 用户原话「完成或实现了，可以折叠起来自动归档」：要的不是「看不到已完成」，
  // 而是「已完成的不该和进行中的一样占地方」。折叠点选在**组**而非任务——组是
  // 状态机的产物，折它就是换视觉权重，一条信息都不丢。
  const grouped = groupTasks(tasksAll, sec.group_by ?? "status");
  const st = sec.group_by ?? "status";
  const settled = (g) => st === "status" && (g === "done" || g === "cancelled");
  let shown = 0;
  for (const [g, arr] of grouped) {
    const label = st === "status" ? (BOARD_LABEL[g] ?? g) : g;
    const shut = folded(sec.id, g, settled(g));
    body.appendChild(tgroupHead(sec.id, g, label, arr.length, shut, wsDist(arr)));
    // v13：**收起态也要露出最新那一条**。落定组默认折叠是对的（已完成不该和进行中
    // 的一样占地方），但「折成一行只剩数字」正是用户那句「任务做完了，看板为什么不
    // 体现」的直接来源——他刚交付的那条被折进「完成 30」里，等于没有反馈。
    // 为什么不用时间窗（「N 分钟内算刚做完」）：窗口是任意数，10 分钟前和 21 分钟前
    // 没有本质区别，用户隔天回来照样看不到自己昨天做完的活。**预览一条最新**没有
    // 这个缺陷：任何时刻都能看到「这一组最近发生了什么」，代价只有一行高度。
    if (shut) {
      if (arr.length) {
        const preview = [...arr].sort((a, b) =>
          String(b.updated_at ?? "").localeCompare(String(a.updated_at ?? "")))[0];
        const row = taskRow(preview);
        row.classList.add("tpreview");
        body.appendChild(row);
      }
      continue;
    }
    for (const t of arr) {
      if (shown >= limit) break;
      body.appendChild(taskRow(t));
      shown += 1;
    }
  }
  if (tasksAll.length > shown) {
    body.appendChild(el("div", "empty", `…还有 ${tasksAll.length - shown} 个任务（切「全部/未完成」或调高 limit）`));
  }
  return wrap;
}

// ── F1：折叠状态（组级）──────────────────────────────────────────────────────
// 为什么不用「显示全部/隐藏已完成」两个按钮：那是全局开关，切一次就丢了上下文；
// 组级折叠记在 localStorage，刷新后还是你上次留下的样子（看板是长期视图，
// 每次打开都要重新摆一遍等于没有记忆）。
// 缺省值由调用方给：落定组默认收起，活跃组默认展开——**默认值本身就是设计**。
const FOLD_KEY = "ra.boardfold";
let boardFold = {};
try {
  const raw = JSON.parse(localStorage.getItem(FOLD_KEY) || "{}");
  if (raw && typeof raw === "object" && !Array.isArray(raw)) boardFold = raw;
} catch { boardFold = {}; /* 隐私模式/坏值：按全展开处理，不阻断渲染 */ }

function foldKey(secId, g) { return `${secId}#${g}`; }
function folded(secId, g, dflt) {
  const k = foldKey(secId, g);
  return k in boardFold ? !!boardFold[k] : !!dflt;
}
function setFold(secId, g, v) {
  boardFold[foldKey(secId, g)] = !!v;
  try { localStorage.setItem(FOLD_KEY, JSON.stringify(boardFold)); } catch { /* 隐私模式：忽略 */ }
}

// v13：把一组条目按工作区聚成「标签×条数」，条数降序，最多显示 max 个，其余折成 +N。
// 为什么要它：折叠组头只有「完成 29」一个数字时，用户仍然不知道里面有没有自己项目的活——
// 而这正是他抱怨的那件事（「任务做完了，看板为什么不体现」）。分布摘要让**收起态也带归属信息**。
function wsDist(items, max = 3) {
  const cnt = new Map();
  for (const it of items) {
    const k = wsTagOf(it);
    if (!k) continue;                 // 归属未知不参与统计，也不编标签
    cnt.set(k, (cnt.get(k) ?? 0) + 1);
  }
  if (!cnt.size) return "";
  const mine = boardMine();
  const keys = [...cnt.keys()].sort((a, b) => {
    const am = a === mine ? 0 : 1, bm = b === mine ? 0 : 1;
    if (am !== bm) return am - bm;
    return cnt.get(b) - cnt.get(a) || a.localeCompare(b);
  });
  const shown = keys.slice(0, max);
  const rest = keys.length - shown.length;
  const html = shown.map((k) => `<span class="tdws${k === mine ? " mine" : ""}">` +
    `${boardEsc(k)}<i>${cnt.get(k)}</i></span>`).join("");
  return html + (rest > 0 ? `<span class="tdws more" title="${boardEsc(keys.slice(max).join(" / "))}">+${rest}</span>` : "");
}

// 组头：可点折叠。收起时把该组的条目数放在最右——折叠不等于隐藏，先告诉你「里面有几条」。
// v13：`dist` 非空时在条数左侧补一段工作区分布（收起态也显示，这是本轮的核心）。
function tgroupHead(secId, g, label, n, shut, dist) {
  const head = el("div", "tgroup" + (shut ? " shut" : ""));
  head.title = shut ? `展开「${label}」（${n} 条）` : `收起「${label}」`;
  head.innerHTML = `<span class="tchev">${shut ? "▸" : "▾"}</span>` +
    `<span class="tdot ${boardEsc(g)}"></span><span>${boardEsc(label)}</span>` +
    (dist ? `<span class="tdist">${dist}</span>` : "") +
    `<span class="tcnt">${n}</span>`;
  head.onclick = () => { setFold(secId, g, !shut); refreshSections(); };
  return head;
}

function taskRow(t) {
  const row = document.createElement("div");
  row.className = `trow st-${boardEsc(t.status)}`;
  const done = Number(t.progress?.done ?? 0), total = Number(t.progress?.total ?? 0);
  const ratio = total > 0 ? `${done}/${total}` : "";
  const pct = total > 0 ? Math.round((done / total) * 100) : 0;
  const prio = t.priority ?? "P2";
  const res = t.resourcing ?? {};
  const verdict = t.evaluation?.verdict ?? "none";
  const bits = [];
  if (res.effort) bits.push({ small: "轻", medium: "中", large: "重" }[res.effort] ?? res.effort);
  if (res.model && res.model !== "current") bits.push(res.model === "cheap" ? "省模型" : res.model === "strong" ? "强模型" : res.model);
  if ((res.skills ?? []).length) bits.push(`部门 ${res.skills.length}`);
  if ((res.tools ?? []).length) bits.push(`工具 ${res.tools.length}`);
  if ((t.checks ?? []).length) bits.push(`验收 ${t.checks.length}`);
  // v13：工作区徽章。**「本项目」档下不渲染**——那一档里每条都是本工作区，
  // 印出来只是噪声，还会把本来就不宽的标题栏挤得更窄。归属未知的老条目也不印。
  const ws = wsTagOf(t);
  const wsChip = (ws && boardScope !== "mine")
    ? `<span class="tws${ws === boardMine() ? " mine" : ""}" title="这条任务在哪个工作区产生">${boardEsc(ws)}</span>`
    : "";
  row.innerHTML =
    `<div class="tline"><span class="tprio p-${boardEsc(prio)}">${boardEsc(prio)}</span>` +
    wsChip +
    `<span class="ttitle" title="${boardEsc(t.title)}">${boardEsc(t.title)}</span>` +
    (verdict !== "none" ? `<span class="tverdict v-${boardEsc(verdict)}">${boardEsc(VERDICT_LABEL[verdict] ?? verdict)}</span>` : "") +
    (ratio ? `<span class="tratio">${ratio}</span>` : "") +
    `<button class="tref" title="在输入框引用这个任务（@T）">@</button></div>` +
    (total > 0 ? `<div class="tbar"><i style="width:${pct}%"></i></div>` : "") +
    (bits.length ? `<div class="tmeta">${bits.map((b) => `<span>${boardEsc(b)}</span>`).join("")}</div>` : "") +
    `<div class="tdetail" hidden></div>`;
  row.querySelector(".tref").onclick = (e) => { e.stopPropagation(); mentionRef(t.id); };
  row.querySelector(".tline").onclick = () => toggleTaskDetail(row, t);
  if (boardOpen.has(t.id)) {
    const box = row.querySelector(".tdetail");
    fillTaskDetail(box, t);
    box.hidden = false;
  }
  return row;
}

function checkLine(c) {
  const st = c.status ?? "pending";
  const ev = c.evidence ?? {};
  const bits = [];
  if (ev.exit_code != null) bits.push(`exit=${ev.exit_code}`);
  if (ev.hits != null) bits.push(`hits=${ev.hits}`);
  if (ev.exists != null) bits.push(ev.exists ? "存在" : "缺失");
  if (ev.timeout_secs != null) bits.push(`超时 ${ev.timeout_secs}s`);
  return `<li><span class="cdot ${boardEsc(st)}"></span>${boardEsc(c.name ?? c.id)}` +
    `<span class="cmeta">${boardEsc(c.kind ?? "")}${bits.length ? " · " + boardEsc(bits.join(" ")) : ""}</span></li>`;
}

function fillTaskDetail(box, t) {
  const plan = (t.plan ?? []).map((s) => `<li><span class="pdot ${boardEsc(s.status)}"></span>${boardEsc(s.step)}</li>`).join("");
  const acc = (t.acceptance ?? []).map((a) => `<li>${boardEsc(a)}</li>`).join("");
  const checks = (t.checks ?? []).map(checkLine).join("");
  const ev = t.evaluation ?? {};
  const res = t.resourcing ?? {};
  // G5（I-20261108-04 第 3 条）：**结论放最前**，且分两条线。
  // 用户原话：「任务完成后其实应该是两条线。一条是确认任务是否专业的、按照计划、需求
  // 完成了。另一条是需要把任务的结果通俗化的呈现出来，这才是看板的终极意义」。
  // 所以顺序是：先说给人听的（一句话，白话），再说给 agent 看的（专业结论）。
  // 之前的详情页一上来是「目标 / 计划 / 验收标准」——全是过程，看完仍不知道做成了没有。
  const outs = t.outcomes ?? {};
  const briefTxt = String(outs.brief ?? "").trim();
  const proTxt = String(outs.pro ?? "").trim();
  const outcomeBlock = (briefTxt || proTxt)
    ? `<div class="dsec doutcome">` +
      (briefTxt ? `<b>结果</b><p class="obrief">${boardEsc(briefTxt)}</p>` : "") +
      (proTxt ? `<details class="opro"><summary>专业结论（给 agent 归纳用）</summary><p>${boardEsc(proTxt)}</p></details>` : "") +
      `</div>`
    : t.status === "done"
      ? `<div class="dsec doutcome"><b>结果</b><p class="obrief empty-inline">这件已完成，但没有留下结论——「完成了」不等于「结果是什么」</p></div>`
      : "";
  box.innerHTML =
    outcomeBlock +
    (t.goal ? `<div class="dsec"><b>目标</b><p>${boardEsc(t.goal)}</p></div>` : "") +
    (plan ? `<div class="dsec"><b>计划</b><ul class="dplan">${plan}</ul></div>` : "") +
    (acc ? `<div class="dsec"><b>验收标准</b><ul class="dacc">${acc}</ul></div>` : "") +
    (checks
      ? `<div class="dsec"><b>可执行检查</b><ul class="dchecks">${checks}</ul>` +
        `<p class="dverdict v-${boardEsc(ev.verdict ?? "none")}">结论 ${boardEsc(VERDICT_LABEL[ev.verdict] ?? ev.verdict ?? "未跑")}` +
        (ev.pass || ev.fail || ev.skip ? ` · pass ${Number(ev.pass)} / fail ${Number(ev.fail)} / skip ${Number(ev.skip)}` : "") +
        (ev.ran_at ? ` · ${boardEsc(ev.ran_at)}` : "") + `</p></div>`
      : "") +
    `<div class="dsec"><b>人财物</b><p>模型 ${boardEsc(res.model ?? "current")} · 力度 ${boardEsc(res.effort ?? "medium")}` +
    (res.skills?.length ? ` · 技能 ${boardEsc(res.skills.join("、"))}` : "") +
    (res.tools?.length ? ` · 工具 ${boardEsc(res.tools.join("、"))}` : "") + `</p></div>` +
    (t.progress?.note ? `<div class="dsec"><b>进度备注</b><p>${boardEsc(t.progress.note)}</p></div>` : "") +
    ((t.artifacts ?? []).length
      ? `<div class="dsec"><b>产物</b><p>${t.artifacts.map((a) => `<a href="/files/${encodeURI(a)}" target="_blank" rel="noopener">${boardEsc(a.split("/").pop())}</a>`).join(" · ")}</p></div>`
      : "") +
    `<div class="dsec"><span class="did">${boardEsc(t.id)}</span></div>`;
}

function toggleTaskDetail(row, t) {
  const box = row.querySelector(".tdetail");
  if (!box) return;
  if (!box.hidden) { box.hidden = true; boardOpen.delete(t.id); return; }
  fillTaskDetail(box, t);
  box.hidden = false;
  boardOpen.add(t.id);
}

// ── metrics 章节（评价体系可见）───────────────────────────────────────────────
// ── F4：验收趋势（不新增数据层）──────────────────────────────────────────────
// 「metrics 历史趋势」此前被列为待办，前提假设是「要按天快照（数据层扩张）」。
// 重新想一遍：看板里每个任务自带 `updated_at` 与 `evaluation.ran_at`，
// 已经足够回答「**最近几天的验收通过率在变好还是变坏**」——缺的不是数据，是切片。
// 所以这里不加文件、不加表、不加定时任务，只按天把已有字段分桶。
//
// 诚实的边界：这不是「历史快照」，而是「**按最近更新时间的分布**」——一个任务
// 只计入它最后一次更新的那天，所以它回答「近期完成的活里验收质量如何」，
// 不能回答「上周三那天看板上有多少个 pass」（那才需要快照）。UI 上照这个口径写文案。
function verdictTrend(tasks, days) {
  const now = Date.now();
  const buckets = [];
  for (let i = days - 1; i >= 0; i -= 1) {
    const start = new Date(now - i * 86400000);
    buckets.push({
      label: `${start.getMonth() + 1}/${start.getDate()}`,
      key: start.toISOString().slice(0, 10),
      pass: 0, fail: 0, pending: 0, none: 0, total: 0,
    });
  }
  const idx = new Map(buckets.map((b, i) => [b.key, i]));
  for (const t of tasks) {
    const stamp = t.evaluation?.ran_at || t.updated_at || "";
    const key = String(stamp).slice(0, 10);
    const i = idx.get(key);
    if (i == null) continue;
    const b = buckets[i];
    b.total += 1;
    const v = t.evaluation?.verdict;
    if (v === "pass") b.pass += 1;
    else if (v === "fail") b.fail += 1;
    else if (v === "pending") b.pending += 1;
    else b.none += 1;
  }
  return buckets;
}

// 趋势条形：高度=当日任务数，实心比例=其中验收通过的比例。
// 不用折线——7 个点的折线看不出趋势，反而像噪声；柱子的高度差一眼就能比。
function trendBlock(tasks) {
  const days = 7;
  const buckets = verdictTrend(tasks, days);
  const peak = Math.max(1, ...buckets.map((b) => b.total));
  const box = el("div", "bw-sec bw-trend");
  const judged = buckets.reduce((s, b) => s + b.pass + b.fail, 0);
  const passed = buckets.reduce((s, b) => s + b.pass, 0);
  box.innerHTML = `<b>近 7 天验收</b><span class="bw-trend-hint">${
    judged ? `已判定 ${judged} 件 · 通过 ${passed}（${Math.round((passed / judged) * 100)}%）` : "近 7 天没有跑过验收的任务"
  }</span>`;
  const bars = el("div", "bw-bars");
  for (const b of buckets) {
    const h = Math.round((b.total / peak) * 100);
    const okRate = b.total ? Math.round((b.pass / b.total) * 100) : 0;
    const col = el("div", "bw-bar-col");
    col.title = `${b.label}：${b.total} 件更新 · 通过 ${b.pass} · 不通过 ${b.fail} · 待人判 ${b.pending} · 未跑 ${b.none}`;
    col.innerHTML = `<span class="bw-bar-n">${b.total || ""}</span>` +
      `<span class="bw-bar-t" style="height:${Math.max(h, b.total ? 6 : 0)}%">` +
      `<i style="height:${okRate}%"></i></span>` +
      `<span class="bw-bar-l">${boardEsc(b.label)}</span>`;
    bars.appendChild(col);
  }
  box.appendChild(bars);
  const note = el("p", "bw-trend-note");
  note.textContent = "口径：按任务最后更新时间分桶，只计入最后一次状态——回答「近期完成的活质量如何」，不是「某天看板上有多少件」。";
  box.appendChild(note);
  return box;
}
function metricTiles(tasks) {
  const byVerdict = { none: 0, pending: 0, pass: 0, fail: 0 };
  let withChecks = 0, withAcc = 0, checkedPass = 0;
  for (const t of tasks) {
    const v = t.evaluation?.verdict;
    byVerdict[v && byVerdict[v] != null ? v : "none"] += 1;
    if ((t.checks ?? []).length) withChecks += 1;
    if ((t.acceptance ?? []).length) withAcc += 1;
    if (v === "pass") checkedPass += 1;
  }
  return [
    { id: "verdict", label: "验收", value: `${checkedPass}/${tasks.length}`, hint: "通过 / 总任务" },
    { id: "acceptance", label: "有标准", value: `${withAcc}/${tasks.length}`, hint: "写了验收标准" },
    { id: "by_status", label: "有检查", value: `${withChecks}/${tasks.length}`, hint: "登记了可执行检查" },
    {
      id: "by_skill", label: "不通过", value: String(byVerdict.fail),
      hint: `未跑 ${byVerdict.none} · 通过 ${byVerdict.pass}`,
    },
  ];
}

function renderMetricsSection(sec) {
  const wrap = el("section", "board-sec");
  wrap.dataset.kind = "metrics";
  wrap.dataset.sec = sec.id;
  const tasks = boardData?.board?.tasks ?? [];
  wrap.appendChild(sectionHead(sec, tasks.length ? `${tasks.length} 个任务` : "", false));
  const body = el("div", "sec-body metrics");
  body.id = `sec-${sec.id}-body`;
  wrap.appendChild(body);
  if (!tasks.length) {
    body.appendChild(el("div", "empty", "还没有任务，指标暂无意义"));
    return wrap;
  }
  const wanted = new Set(sec.metrics ?? []);
  const tiles = metricTiles(tasks).filter((t) => !wanted.size || wanted.has(t.id));
  for (const tile of tiles) {
    const row = el("div", "metric");
    row.innerHTML = `<span class="mlabel">${boardEsc(tile.label)}</span>` +
      `<span class="mvalue">${boardEsc(tile.value)}</span>` +
      `<span class="mhint">${boardEsc(tile.hint)}</span>`;
    body.appendChild(row);
  }
  return wrap;
}

// ── 灵感显示状态推导（J1）────────────────────────────────────────────────────
// 背景：G1 把灵感的写权限收归用户后，agent 侧不再有 `insp-adopt`，采纳关联改记
// 在**任务一侧**的 `task.inspirations`（一个事实只存一处）。但此前 UI 分组仍只读
// `inspiration.status`，而 `adopted` 这个值**再没有任何通道能写进去**（agent 不许写、
// UI 只有「归档」），于是灵感永远停在「待议」——任务都做完了，左边还挂着 7 条待议。
//
// 修法不是把状态写回灵感对象（那会重新引入「两边说法不一致」），而是让 UI **推导**：
// 有任务引用它 → 已采纳；否则用存下来的 status。这样任务侧仍是唯一真源，
// 灵感对象的 status 只表达用户的显式动作（归档），不再承担「被采纳」这个派生事实。
//
// 为什么推导而不回填：回填需要一次性的迁移脚本，且此后每建一个任务就得同步一次，
// 漏一次就重新不一致；推导每次渲染都算，天然不会过期。
function deriveInspState(items) {
  const claimed = new Map(); // I-ID → 引用它的任务数
  for (const t of boardData?.board?.tasks ?? []) {
    for (const iid of t.inspirations ?? []) claimed.set(iid, (claimed.get(iid) ?? 0) + 1);
  }
  return items.map((it) => {
    // 归档是用户的显式动作，优先级高于推导——归档了就是不想再看。
    if (it.status === "archived") return { ...it, derived: false };
    const n = claimed.get(it.id) ?? 0;
    if (!n) return { ...it, derived: false };
    return { ...it, status: "adopted", derived: true, taskCount: n };
  });
}

// ── inspirations 章节 ───────────────────────────────────────────────────────
function renderInspSection(sec) {
  const wrap = el("section", "board-sec grow");
  wrap.dataset.kind = "inspirations";
  wrap.dataset.sec = sec.id;
  const items = deriveInspState(boardData?.board?.inspirations ?? []).filter(scopeVisible);
  wrap.appendChild(sectionHead(sec, `${items.filter((i) => i.status === "open").length} 待议 / ${items.length}`, false));
  // v12：灵感侧也要档位开关——用户在自己项目窗口记的灵感，就该在本项目视图里看得见。
  wrap.appendChild(scopeBar());

  const add = el("div", "insp-add");
  const input = document.createElement("input");
  input.id = `insp-input-${sec.id}`;
  input.maxLength = 2000;
  // G2：重绘后把未提交的草稿还进新输入框（submit 成功后快照为空，故不会复活旧字）。
  const draft = inspDrafts.get(sec.id);
  if (draft) input.value = draft;
  input.placeholder = "记一条灵感，回车保存";
  const save = document.createElement("button");
  save.id = `insp-save-${sec.id}`;
  save.title = "保存灵感";
  save.textContent = "＋";
  add.append(input, save);
  wrap.appendChild(add);

  const body = el("div", "sec-body");
  body.id = `sec-${sec.id}-body`;
  wrap.appendChild(body);

  const submit = async () => {
    const text = input.value.trim();
    if (!text) return;
    input.disabled = true;
    try {
      await boardReq("POST", "/api/board", { op: "inspiration.add", text });
      input.value = "";
      await refreshBoard();
    } catch (err) {
      setStatus("灵感保存失败: " + err.message);
    } finally {
      input.disabled = false;
      input.focus();
    }
  };
  save.onclick = submit;
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); submit(); }
  });

  if (!items.length) {
    body.appendChild(el("div", "empty", "把零碎想法记在这里，需要时在对话里 @ 引入"));
    return wrap;
  }
  // F1：灵感按状态分三层折叠——待议（open）永远全展，已采纳/已归档各自折成一行。
  // 用户原话「灵感完成或实现了，可以折叠起来自动归档」。已采纳的灵感已经是任务的
  // 来源记录，它的价值从「待我想」变成「可查证」，不该继续占据等待被看的位置。
  const groups = { open: [], adopted: [], archived: [] };
  for (const it of items) (groups[it.status] ?? groups.open).push(it);
  for (const st of ["open", "adopted", "archived"]) {
    const arr = groups[st] ?? [];
    if (!arr.length) continue;
    const shut = folded(sec.id, st, st !== "open");
    const head = tgroupHead(sec.id, st, INSP_LABEL[st] ?? st, arr.length, shut);
    head.classList.add("insp-g");
    body.appendChild(head);
    if (shut) continue;
    for (const it of arr) body.appendChild(inspRow(it));
  }
  return wrap;
}

// 单条灵感（折叠分组后需要按行复用——原来内联在渲染循环里）
function inspRow(it) {
  const row = el("div", `irow st-${boardEsc(it.status)}`);
  const tags = (it.tags ?? []).map((t) => `<span class="itag">${boardEsc(t)}</span>`).join("");
  row.innerHTML =
    `<div class="iline"><span class="itext" title="${boardEsc(it.text)}">${boardEsc(it.text)}</span>` +
    `<button class="iref" title="在输入框引用（@I）">@</button>` +
    `<button class="iedit" title="改这条灵感（Enter 保存 / Esc 取消）">✎</button>` +
    `<button class="irm" title="删除这条灵感">🗑</button>` +
    `<button class="iarc" title="${it.status === "archived" ? "恢复为待议" : "归档"}">${it.status === "archived" ? "↺" : "✓"}</button></div>` +
    (tags || it.status !== "open" ? `<div class="imeta">${tags}<span class="ist">${INSP_LABEL[it.status] ?? it.status}${it.derived ? ` · ${it.taskCount} 个任务` : ""}</span></div>` : "");
  row.querySelector(".iref").onclick = (e) => { e.stopPropagation(); mentionRef(it.id); };
  row.querySelector(".iedit").onclick = (e) => { e.stopPropagation(); editInspiration(it, row); };
  row.querySelector(".irm").onclick = (e) => { e.stopPropagation(); removeInspiration(it, row); };
  row.querySelector(".iarc").onclick = async (e) => {
    e.stopPropagation();
    const next = it.status === "archived" ? "open" : "archived";
    try { await boardReq("POST", "/api/board", { op: "inspiration.set", id: it.id, status: next }); refreshBoard(); }
    catch (err) { setStatus("归档失败: " + err.message); }
  };
  return row;
}

// ── D 轮：灵感可编辑 / 可删除（I-20261107-02）───────────────────────────────
// 原话：「任务看板和灵感看板没有可编辑的功能吗，比如可以删除掉。回滚的时候自动
// 保留之前的文字，否则都要再重新打一遍。」
// 三条取舍：
// 1. **就地改而不是开编辑器**：一条灵感最多 2000 字，把文本行换成 input 就够；
//    开抽屉/模态会引入「到底存了什么」的额外状态面。
// 2. **删除两步且行内确认**：已采纳（adopted）的灵感是任务来源，一次误点就断链。
//    不用 window.confirm——它会遮住列表上下文，用户看不见自己在删哪一条。
// 3. **不做乐观更新**：看板是轮询源，本地先改、服务端后到的写法会让「没删掉」
//    与「删掉了」在下一轮刷新时来回跳。改完即 refreshBoard。
async function editInspiration(it, row) {
  const textEl = row.querySelector(".itext");
  if (!textEl || textEl.dataset.editing === "1") return;
  textEl.dataset.editing = "1";
  const input = document.createElement("input");
  input.className = "insp-edit";
  input.maxLength = 2000;
  input.value = it.text;
  textEl.replaceWith(input);
  input.focus();
  input.setSelectionRange(input.value.length, input.value.length);
  let done = false;
  const finish = async (save) => {
    if (done) return;
    done = true;
    const next = input.value.trim();
    if (save && next && next !== it.text) {
      try {
        await boardReq("POST", "/api/board", { op: "inspiration.edit", id: it.id, text: next });
      } catch (err) {
        setStatus("灵感修改失败: " + err.message);
      }
    }
    await refreshBoard();
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); finish(true); }
    else if (e.key === "Escape") { e.preventDefault(); finish(false); }
  });
  input.addEventListener("blur", () => finish(true));
}

async function removeInspiration(it, row) {
  const adopted = it.status === "adopted";
  const old = row.querySelector(".insp-confirm");
  if (old) old.remove();
  const bar = document.createElement("div");
  bar.className = "insp-confirm";
  bar.innerHTML = `<span>${adopted ? "它已被任务采纳，删掉会断掉任务来源链。" : "确认删除这条灵感？"}</span>` +
    `<button class="yes">删除</button><button class="no">取消</button>`;
  row.appendChild(bar);
  bar.querySelector(".no").onclick = () => bar.remove();
  bar.querySelector(".yes").onclick = async () => {
    bar.remove();
    try {
      await boardReq("POST", "/api/board", { op: "inspiration.rm", id: it.id, force: adopted });
    } catch (err) {
      setStatus("删除失败: " + err.message);
    }
    await refreshBoard();
  };
}
// @引用：把 ID 以纯文本形式写进输入框（不引入隐藏元数据），agent 侧按 `@T-…` / `@I-…`
// 解析——这样引用在 trace 与回滚语义里就是普通文本，不新增任何事件类型。
function mentionRef(id) {
  const box = $("input");
  if (!box) return;
  const token = `@${id} `;
  const cur = box.value;
  const pos = box.selectionStart ?? cur.length;
  const needSpace = pos > 0 && !/\s$/.test(cur.slice(0, pos));
  box.value = cur.slice(0, pos) + (needSpace ? " " : "") + token + cur.slice(pos);
  box.dispatchEvent(new Event("input"));
  box.focus();
  const at = (box.value.length - cur.length) + pos + (needSpace ? 1 : 0) + token.length;
  box.setSelectionRange(at, at);
}

// G2（I-20261108-04 第 2 条）：记灵感的草稿与焦点不能被轮询重绘带走。
// 原话「灵感在 agent 工作的时候，用户记录的时候会被刷新，这样是不对的，这样会影响
// 用户记录灵感」。病灶就在下面那句 root.innerHTML = ""：整个章节连同输入框一起被销毁
// 重建，20s 一次的 boardTick 于是变成「打到一半的字突然没了」——而且丢的是**用户正在
// 产生的内容**，比看到旧看板严重得多。
// 修法只做一件事：重绘前把草稿与焦点位置摘出来，重绘后原样还回去。不猜内容、不做
// 乐观更新、不改任何提交语义——只是不让 DOM 重建顺手带走用户的输入。
// 草稿存内存不存 localStorage：灵感一旦存盘就该从草稿里消失，跨刷新残留会让用户
// 以为「我记过了」。真正的持久化仍只有「回车提交」这一条路径。
// 快照取自**活的 DOM**而不是事件监听：提交成功后代码把 input.value 清空，快照随之
// 变空，草稿自然消失——不需要在提交路径上再补一行清理（少一处会忘的同步点）。
const inspDrafts = new Map();   // sec.id → 未提交的输入
let inspFocus = null;           // 重绘前焦点所在的灵感输入框（含选区），重绘后还原

function refreshSections() {
  const root = $("board-sections");
  if (!root) return;
  // ① 先摘草稿：现存输入框的值就是用户还没提交的内容。
  for (const box of root.querySelectorAll(".insp-add input")) {
    if (!box.id) continue;
    const secId = box.id.replace(/^insp-input-/, "");
    if (box.value) inspDrafts.set(secId, box.value);
    else inspDrafts.delete(secId);
  }
  // ② G3（I-20261108-04 第 2 条，第二个出口）：行内编辑框（.insp-edit）也会被下一行
  // 的 root.innerHTML = "" 连根拔掉。用户点 ✎ 改到一半、20s 轮询一到，不只是字没了
  // ——input 被销毁，它身上的 blur/keydown 监听随之消失，连「想存」都没有入口了。
  // 这与「记新灵感被打断」是同一类伤害：丢的是用户正在产生的内容。G2 只堵住了新增
  // 输入框这个出口，行内编辑这条路一直敞着。
  //
  // 修法取最保守的一种：**正在编辑就跳过本轮重绘**。不重建 = 输入框与它的监听都还在，
  // 用户按 Enter/Esc/blur 仍走原来的保存路径；等编辑结束，finish() 自己会调
  // refreshBoard()，不会漏掉这一刻的服务端变化。相比之下「重建后再把输入框装回去」
  // 要重新绑一套监听、还要处理「这期间那条灵感被别处删了」，多出的状态面换不来更多。
  // 时机代价：编辑期间看板不跟着刷新（一般几秒），但用户此刻看的是那一行，不是全表。
  const active = document.activeElement;
  if (active && active.classList && active.classList.contains("insp-edit")) {
    // 不需要另记「待补刷新」：这条编辑的 finish() 结尾本就 await refreshBoard()，
    // 那一刻本函数会正常往下走，期间的更新自然补上。
    return;
  }
  // ③ 再摘焦点：只有焦点在灵感输入框里才记（避免抢别处的焦点）。
  inspFocus = active && typeof active.id === "string" && active.id.startsWith("insp-input-")
    ? { id: active.id, start: active.selectionStart, end: active.selectionEnd }
    : null;
  root.innerHTML = "";
  const secs = boardSections();
  if (!secs.length) {
    root.appendChild(el("div", "empty", "看板布局为空（layout.sections 无合法章节）"));
    return;
  }
  const renderers = {
    tasks: renderTasksSection,
    metrics: renderMetricsSection,
    inspirations: renderInspSection,
  };
  for (const sec of secs) {
    const fn = renderers[sec.kind];
    if (fn) root.appendChild(fn(sec));
  }
  // 兜底：布局里若漏了任务或灵感章节，补一个可见提示（不静默少一块）
  const kinds = new Set(secs.map((s) => s.kind));
  for (const k of ["tasks", "inspirations"]) {
    if (!kinds.has(k)) {
      root.appendChild(el("div", "empty", `布局未包含 ${KIND_LABEL[k]} 章节（\`board.py layout --file\` 可调整）`));
    }
  }
  // G4（I-20261108-04 第 2 条的收尾）：把焦点与选区真的还回去。
  // G2 当初的注释写着「重绘后原样还回去」，但只落地了草稿回填——`inspFocus` 被赋值
  // 两次、**从未被读取**，是个只写不读的死变量：字还在，光标不在，用户得重新点进
  // 输入框才能接着打。对「记录碎片想法」这个动作来说，光标位置就是上下文。
  // 放在章节装配之后：输入框必须先存在，才谈得上聚焦。
  if (inspFocus) {
    const box = document.getElementById(inspFocus.id);
    if (box) {
      box.focus();
      try { box.setSelectionRange(inspFocus.start, inspFocus.end); } catch (_) {}
    }
    inspFocus = null;   // 一次性的：还原过就清掉，避免影响下次重绘
  }
  // D 轮：弹窗与侧栏共用同一份 boardData / layout——一次 GET 喂两处，不额外请求。
  renderBoardWin();
  const foot = $("board-win-foot");
  if (foot) {
    const sum = boardData?.summary?.tasks;
    const bs = sum?.by_status ?? {};
    foot.textContent = `共 ${sum?.total ?? 0} 个任务 · 未完成 ${
      (bs.planning ?? 0) + (bs.doing ?? 0) + (bs.verifying ?? 0)} · 验收通过 ${
      sum?.by_verdict?.pass ?? 0}`;
  }
}

async function refreshBoard() {
  try {
    boardData = await boardReq("GET", "/api/board");
    refreshSections();
    // 车道条随看板刷新重绘：任务的新建/推进/收口（一律由 agent 经 board.py 完成）
    // 会改变车道清单，前端只是跟着重排。必须在 boardData 赋值之后——laneList() 读它。
    if (typeof renderLanes === "function") renderLanes();
  } catch (e) {
    const root = $("board-sections");
    if (root) root.innerHTML = `<div class="empty">看板读取失败: ${boardEsc(e.message)}</div>`;
  }
}

function boardTick() {
  // 页面不可见时不拉（后台标签页省 CPU）；切回可见立即补一次。
  if (document.visibilityState === "visible") refreshBoard();
}

document.addEventListener("visibilitychange", boardTick);

refreshBoard();
boardTimer = setInterval(boardTick, 20000);

// D 轮：弹窗初始化放在这里而不是文件顶部——`initBoardWin` 会读 localStorage 并按需
// 立刻 open（open 会调 refreshBoard），必须等全部函数声明与常量就位后再绑。
initBoardWin();

// ── D 轮：看板弹窗（"任务看板是一个标签，点进去是活的界面"）───────────────
// 设计取舍（对齐 I-20261107-03）：
// 1. **同源不同壳**：弹窗复用侧栏同一份 `layout.sections` 与同一批数据——不是第二套
//    看板实现。侧栏是「一眼看进度」的窄栏，弹窗是「能摊开细看」的宽面：同一个任务
//    在侧栏只有一行摘要，在弹窗里展开完整的人财物 / 计划 / 验收 / 检查 / 产物。
// 2. **一个弹窗分标签**：按 kind 出标签（任务 / 指标 / 灵感），任务标签内再按 layout
//    的 group_by 分组。多开窗要额外处理 z-order 与状态同步，收益不抵成本。
// 3. **活的**：跟随既有 boardTimer 轮询；打开弹窗只多一次 GET，之后共用同一份数据。
// 4. **可关可记**：Esc / ✕ / 点遮罩关闭；开着状态存 localStorage，刷新页面还记得。
const BOARDWIN_KEY = "ra.boardwin";

function boardWinEl() { return $("board-win"); }
function boardWinOpen() { const w = boardWinEl(); return !!w && !w.hidden; }

// 弹窗任务卡：**默认展开**详情（与侧栏相反——侧栏省空间要折叠，弹窗就是要看细节）。
function winTaskCard(t) {
  const card = el("div", `bw-card st-${boardEsc(t.status)}`);
  const done = Number(t.progress?.done ?? 0), total = Number(t.progress?.total ?? 0);
  const pct = total > 0 ? Math.round((done / total) * 100) : 0;
  const res = t.resourcing ?? {};
  const skillTags = (res.skills ?? []).map((s) => `<span class="bw-skill">${boardEsc(s)}</span>`).join("");
  const toolTags = (res.tools ?? []).map((s) => `<span class="bw-skill tool">${boardEsc(s)}</span>`).join("");
  const verdict = t.evaluation?.verdict ?? "none";

  const head = el("div", "bw-head");
  head.innerHTML =
    `<span class="tprio p-${boardEsc(t.priority ?? "P2")}">${boardEsc(t.priority ?? "P2")}</span>` +
    `<span class="bw-title">${boardEsc(t.title)}</span>` +
    `<span class="bw-st st-${boardEsc(t.status)}">${boardEsc(BOARD_LABEL[t.status] ?? t.status)}</span>` +
    `<span class="tverdict v-${boardEsc(verdict)}">${boardEsc(VERDICT_LABEL[verdict] ?? verdict)}</span>` +
    // v12：归属标签只在「全部」档且真有标签时显示——本项目档下每条都一样，
    // 印出来只是噪声；归属未知的老条目不显示（不写「未知」，那会被读成「丢失」）。
    (boardScope !== "mine" && t.workspace
      ? `<span class="bw-ws" title="这条任务在哪个工作区产生">${boardEsc(t.workspace)}</span>`
      : "") +
    `<button class="bw-ref" title="在输入框引用这个任务（@T）">@ 引用</button>` +
    `<button class="bw-link" title="查哪几轮对话提过它（读 trace 的 refs 字段）">⌕ 对话</button>`;
  card.appendChild(head);

  if (t.goal) {
    const g = el("p", "bw-goal");
    g.textContent = t.goal;
    card.appendChild(g);
  }
  if (total > 0) {
    const bar = el("div", "bw-bar");
    bar.innerHTML = `<i style="width:${pct}%"></i><span>${done}/${total}（${pct}%）</span>`;
    card.appendChild(bar);
  }

  // 人财物：模型 / 力度 / 部门（技能）/ 工具 / 预算
  const resRow = el("div", "bw-res");
  resRow.innerHTML =
    `<span class="bw-k">模型</span><span>${boardEsc(res.model ?? "current")}</span>` +
    `<span class="bw-k">力度</span><span>${boardEsc({ small: "轻", medium: "中", large: "重" }[res.effort] ?? res.effort ?? "medium")}</span>` +
    (skillTags ? `<span class="bw-k">部门</span><span>${skillTags}</span>` : "") +
    (toolTags ? `<span class="bw-k">工具</span><span>${toolTags}</span>` : "") +
    (Number(res.budget_secs) > 0 ? `<span class="bw-k">预算</span><span>${Number(res.budget_secs)}s</span>` : "");
  card.appendChild(resRow);

  // 计划（含 D 轮 owner：self / subagent / skill —— 谁执行这一步是决策记录）
  const plan = t.plan ?? [];
  if (plan.length) {
    const sec = el("div", "bw-sec");
    const ownerLabel = { self: "本体", subagent: "子代理", skill: "现造技能" };
    sec.innerHTML = `<b>计划</b><ul class="dplan">` + plan.map((s) =>
      `<li><span class="pdot ${boardEsc(s.status)}"></span>${boardEsc(s.step)}` +
      (s.owner && s.owner !== "self"
        ? `<span class="bw-owner o-${boardEsc(s.owner)}">${boardEsc(ownerLabel[s.owner] ?? s.owner)}</span>` : "") +
      `</li>`).join("") + `</ul>`;
    card.appendChild(sec);
  }

  // 分解：子任务一眼可见（parent 只一层，够用且不引入第二套层级系统）
  const kids = (boardData?.board?.tasks ?? []).filter((x) => x.parent === t.id);
  if (kids.length) {
    const sec = el("div", "bw-sec");
    sec.innerHTML = `<b>拆成 ${kids.length} 件</b><div class="bw-kids">` + kids.map((k) =>
      `<span class="bw-kid st-${boardEsc(k.status)}">${boardEsc(k.title)}` +
      `<i>${boardEsc(BOARD_LABEL[k.status] ?? k.status)}</i></span>`).join("") + `</div>`;
    card.appendChild(sec);
  }

  const acc = t.acceptance ?? [];
  if (acc.length) {
    const sec = el("div", "bw-sec");
    sec.innerHTML = `<b>验收标准</b><ul class="dacc">` + acc.map((a) => `<li>${boardEsc(a)}</li>`).join("") + `</ul>`;
    card.appendChild(sec);
  }
  const checks = t.checks ?? [];
  if (checks.length) {
    const ev = t.evaluation ?? {};
    const sec = el("div", "bw-sec");
    sec.innerHTML = `<b>可执行检查</b><ul class="dchecks">` + checks.map(checkLine).join("") + `</ul>` +
      `<p class="dverdict v-${boardEsc(ev.verdict ?? "none")}">结论 ${boardEsc(VERDICT_LABEL[ev.verdict] ?? ev.verdict ?? "未跑")}` +
      (ev.pass || ev.fail || ev.skip ? ` · pass ${Number(ev.pass)} / fail ${Number(ev.fail)} / skip ${Number(ev.skip)}` : "") +
      (ev.ran_at ? ` · ${boardEsc(ev.ran_at)}` : "") + `</p>`;
    card.appendChild(sec);
  }

  // 产物 / 来源灵感 / 上级 / 对话关联（点 ⌕ 现查，不预加载——查一个任务才发一次请求）
  const meta = el("div", "bw-sec bw-meta");
  const arts = t.artifacts ?? [];
  let metaHtml = "";
  if (arts.length) {
    metaHtml += `<div><b>产物</b> ` + arts.map((a) =>
      `<a href="/files/${encodeURI(a)}" target="_blank" rel="noopener">${boardEsc(a.split("/").pop())}</a>`).join(" · ") + `</div>`;
  }
  const insps = t.inspirations ?? [];
  if (insps.length) {
    metaHtml += `<div><b>来源灵感</b> ` + insps.map((i) => `<span class="bw-insp">${boardEsc(i)}</span>`).join(" · ") + `</div>`;
  }
  if (t.parent) {
    metaHtml += `<div><b>上级</b> <span class="bw-insp">${boardEsc(t.parent)}</span></div>`;
  }
  metaHtml += `<div class="bw-links" hidden></div><div class="bw-foot"><span class="did">${boardEsc(t.id)}</span></div>`;
  meta.innerHTML = metaHtml;
  card.appendChild(meta);

  head.querySelector(".bw-ref").onclick = () => { mentionRef(t.id); closeBoardWin(); };
  head.querySelector(".bw-link").onclick = async (e) => {
    const box = meta.querySelector(".bw-links");
    e.target.disabled = true;
    try {
      const v = await boardReq("GET", `/api/board/link?id=${encodeURIComponent(t.id)}`);
      const turns = v.turns ?? [];
      box.hidden = false;
      box.innerHTML = turns.length
        ? `<b>提到它的对话</b> ` + turns.map((x) =>
            `<span class="bw-turn" title="${boardEsc(x.text)}">第 ${Number(x.user_index) + 1} 轮</span>`).join(" ")
        : `<span class="empty-inline">没有对话引用过它（在消息里写 @${boardEsc(t.id)} 即建立关联）</span>`;
    } catch (err) {
      box.hidden = false;
      box.innerHTML = `<span class="empty-inline">查询失败: ${boardEsc(err.message)}</span>`;
    } finally {
      e.target.disabled = false;
    }
  };
  return card;
}

function renderWinTasks(sec, root) {
  const body = el("div", "bw-body");
  let all = (boardData?.board?.tasks ?? []).filter(scopeVisible);
  if (!all.length) {
    body.appendChild(el("div", "empty", boardScope === "mine" && boardMine()
      ? `本项目（${boardMine()}）下暂无任务。切「全部」看整本账。`
      : "还没有任务。把一件成规模的事交代下来，agent 会把它登记到这里。"));
    root.appendChild(body);
    return;
  }
  const groupBy = sec.group_by ?? "status";
  // V14（I-20261115-03）：弹窗里的过滤也走用户点的 boardFilter（原来传 sec.filter，
  // 与侧栏同一个 bug：点「未完成」只换高亮、列表不变）。
  const grouped = groupTasks(all.filter((t) => taskVisible(t, boardFilter ?? "all")), groupBy);
  // V14（I-20261115-03）：**弹窗不受 layout.limit 截断**。
  //
  // 用户原话：「至少在专门点进看板的标签中，它能点了之后它能显示所有任务」。
  // 侧栏是窄栏、limit=30 是合理的省空间手段；弹窗是「摊开细看」的宽面，
  // 截断在这里没有任何收益——只有「翻不到底」的坏处。侧栏的 limit 仍然生效，
  // 两边职责不同：侧栏一眼看进度，弹窗看全部。
  for (const [g, arr] of grouped) {
    const label = groupBy === "status" ? (BOARD_LABEL[g] ?? g) : g;
    const settled = groupBy === "status" && (g === "done" || g === "cancelled");
    const shut = folded("win", g, settled);
    const head = el("div", "bw-group" + (shut ? " shut" : ""));
    head.title = shut ? `展开「${label}」（${arr.length} 条）` : `收起「${label}」`;
    head.innerHTML = `<span class="tchev">${shut ? "▸" : "▾"}</span>` +
      `<span class="tdot ${boardEsc(g)}"></span><span>${boardEsc(label)}</span>` +
      `<span class="tcnt">${arr.length}</span>`;
    head.onclick = () => { setFold("win", g, !shut); renderBoardWin(); };
    body.appendChild(head);
    if (shut) continue;             // 折叠 = 不渲染这一组的卡片，条数仍在组头上
    for (const t of arr) {
      body.appendChild(winTaskCard(t));
    }
  }
  root.appendChild(body);
}

function renderWinMetrics(sec, root) {
  const body = el("div", "bw-body metrics");
  const tasks = boardData?.board?.tasks ?? [];
  if (!tasks.length) {
    body.appendChild(el("div", "empty", "还没有任务，指标暂无意义。"));
    root.appendChild(body);
    return;
  }
  const wanted = new Set(sec.metrics ?? []);
  for (const tile of metricTiles(tasks).filter((t) => !wanted.size || wanted.has(t.id))) {
    const row = el("div", "metric wide");
    row.innerHTML = `<span class="mlabel">${boardEsc(tile.label)}</span>` +
      `<span class="mvalue">${boardEsc(tile.value)}</span>` +
      `<span class="mhint">${boardEsc(tile.hint)}</span>`;
    body.appendChild(row);
  }
  // 指标之外补一屏「按状态的活」——弹窗有地方，侧栏没有。
  const bs = boardData?.summary?.tasks?.by_status ?? {};
  const line = el("p", "bw-statline");
  line.textContent = BOARD_ORDER.map((s) => `${BOARD_LABEL[s]} ${Number(bs[s] ?? 0)}`).join(" · ");
  body.appendChild(line);
  // F4：趋势块（纯前端按 updated_at 分桶，不新增数据层）
  body.appendChild(trendBlock(tasks));
  root.appendChild(body);
}

function renderWinInsp(sec, root) {
  const body = el("div", "bw-body");
  // 必须与左栏 renderInspSection 走同一个推导（J1）。两处若一处推导、一处读原始
  // `inspiration.status`，同一条灵感就会左栏显示「已采纳」、看板窗口显示「待议」——
  // 这不是两个视图的偏好差异，而是同一个事实被算了两遍、其中一遍算错。
  // 「被哪个任务采纳」只存在任务侧 `task.inspirations`，灵感对象的 status 只表达
  // 用户的显式动作（归档），所以任何读它的视图都必须先过 deriveInspState。
  const items = deriveInspState(boardData?.board?.inspirations ?? []).filter(scopeVisible);
  if (!items.length) {
    body.appendChild(el("div", "empty", boardScope === "mine" && boardMine()
      ? `本项目（${boardMine()}）下暂无灵感。切「全部」看整本账。`
      : "把零碎想法记在侧栏，需要时在对话里 @ 引入。"));
    root.appendChild(body);
    return;
  }
  for (const it of items) {
    const row = el("div", `bw-insp-row st-${boardEsc(it.status)}`);
    row.innerHTML =
      `<span class="itext">${boardEsc(it.text)}</span>` +
      `<span class="bw-insp-meta">${(it.tags ?? []).map((x) => `<span class="itag">${boardEsc(x)}</span>`).join("")}` +
      `<span class="ist">${boardEsc(INSP_LABEL[it.status] ?? it.status)}</span></span>` +
      `<span class="bw-insp-act">` +
      `<button class="bw-i-edit" title="改正文">✎</button>` +
      `<button class="bw-i-rm" title="删除">🗑</button>` +
      `<button class="bw-i-ref" title="在输入框引用（@I）">@</button>` +
      `</span>`;
    row.querySelector(".bw-i-ref").onclick = () => { mentionRef(it.id); closeBoardWin(); };
    row.querySelector(".bw-i-edit").onclick = () => editInspiration(it, row);
    row.querySelector(".bw-i-rm").onclick = () => removeInspiration(it, row);
    body.appendChild(row);
  }
  root.appendChild(body);
}

function renderBoardWin() {
  const win = boardWinEl();
  if (!win || win.hidden) return;
  const secs = boardSections();
  const tabs = $("bw-tabs"), root = $("bw-root");
  if (!tabs || !root) return;
  if (!secs.length) {
    tabs.innerHTML = "";
    root.innerHTML = `<div class="empty">看板布局为空（layout.sections 无合法章节）</div>`;
    return;
  }
  // 当前选中标签：记住 kind 而不是 id——改布局删掉某章节后仍能落到同类上。
  const cur = win.dataset.kind || secs[0].kind;
  const active = secs.some((s) => s.kind === cur) ? cur : secs[0].kind;
  win.dataset.kind = active;
  tabs.innerHTML = "";
  for (const s of secs) {
    const b = el("button", s.kind === active ? "on" : "");
    b.textContent = s.title || KIND_LABEL[s.kind] || s.id;
    b.onclick = () => { win.dataset.kind = s.kind; renderBoardWin(); };
    tabs.appendChild(b);
  }
  root.innerHTML = "";
  const sec = secs.find((s) => s.kind === active);
  // v12：弹窗顶部也给档位开关（与左栏同一个 boardScope，切换两处同步）。
  // 弹窗是「摊开细看」的宽面，用户在这里最可能问「这些活分别是哪个项目的」，
  // 所以档位条必须也在——否则同一个开关在窄栏有、宽面没有，观感像两个不同系统。
  const bar = scopeBar();
  bar.classList.add("bw-scope");
  root.appendChild(bar);
  // V14（I-20261115-03）：弹窗也挂状态筛选条（全部 / 未完成 / 已完成）。
  // 弹窗是「摊开细看」的宽面，用户在这里最可能问「哪些还没做完」——侧栏有、
  // 弹窗没有，观感像两个不同系统。同一个 boardFilter，两处同步。
  if (sec.kind === "tasks") {
    const fb = filterBar();
    fb.classList.add("bw-filter");
    root.appendChild(fb);
  }
  const renderers = { tasks: renderWinTasks, metrics: renderWinMetrics, inspirations: renderWinInsp };
  const fn = renderers[sec.kind];
  if (fn) fn(sec, root);
}

function openBoardWin() {
  const win = boardWinEl();
  if (!win) return;
  win.hidden = false;
  try { localStorage.setItem(BOARDWIN_KEY, "1"); } catch { /* 隐私模式：忽略 */ }
  renderBoardWin();
  refreshBoard();  // 打开即拉一次，不等下个轮询周期
}

function closeBoardWin() {
  const win = boardWinEl();
  if (!win) return;
  win.hidden = true;
  try { localStorage.setItem(BOARDWIN_KEY, "0"); } catch { /* 隐私模式：忽略 */ }
}

function initBoardWin() {
  const win = boardWinEl();
  if (!win) return;
  const close = $("bw-close");
  if (close) close.onclick = closeBoardWin;
  win.addEventListener("click", (e) => { if (e.target === win) closeBoardWin(); });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && boardWinOpen()) closeBoardWin();
  });
  const btn = $("btn-board");
  if (btn) btn.onclick = openBoardWin;
  let wantOpen = false;
  try { wantOpen = localStorage.getItem(BOARDWIN_KEY) === "1"; } catch { /* 隐私模式 */ }
  if (wantOpen) openBoardWin();
}

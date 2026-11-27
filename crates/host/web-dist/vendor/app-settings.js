"use strict";
// ── 设置面板（LLM / 工具白名单 / 技能 CRUD）──
const drawer = $("drawer");
// F2：打开设置时按需加载记忆页（连带 loadSkills 一起——两者都是「系统现在怎样」，
// 用户打开设置往往想知道的是这个，而不是马上改配置）。
$("gear").onclick = () => { drawer.classList.add("open"); loadConfig(); loadSkills(); loadMemory(); };
$("dclose").onclick = () => drawer.classList.remove("open");
document.querySelectorAll("#drawer .tabs button").forEach((b) => b.onclick = () => {
  document.querySelectorAll("#drawer .tabs button").forEach((x) => x.classList.toggle("on", x === b));
  for (const t of ["llm", "tools", "skills", "memory", "agent", "im", "publish", "channels"]) $("tab-" + t).hidden = t !== b.dataset.tab;
  // 分发面板按需拉取就绪状态：每次都重新探（用户可能刚在终端装好 sau / 登录完）
  if (b.dataset.tab === "publish") loadPublish();
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
// ── T-13：GitHub 登录态（读 gh 已有凭据，绝不显示 token）──────────────────
// 为什么不是「一键登录」：gh auth login 是阻塞式交互命令（要开浏览器、等授权），
// host 代跑它会一直挂到超时（历史实测 K502）。所以这里只做两件事——
// ① 显示当前登录态（含账号与 scopes，便于确认权限够不够）；
// ② 未登录时给一行**可复制**的命令，用户在自己终端跑完再点「重新检测」。
// 把命令交给用户复制，好过做一个点了没反应的按钮。
async function renderGitAuth() {
  const box = $("git-auth-box");
  if (!box) return;
  box.textContent = "检测中…";
  let v;
  try {
    v = await api("GET", "/api/git/auth");
  } catch (e) {
    box.textContent = "读取失败：" + e.message;
    return;
  }
  // 三种状态各自说清「下一步做什么」，不留悬空
  if (v.status === "logged_in") {
    box.innerHTML =
      `<span style="color:var(--ok)">✓ 已登录</span>　账号 <b>${esc(v.login || "?")}</b>` +
      `<br>凭据来源：${esc(v.token_source || "?")}　协议：${esc(v.git_protocol || "?")}` +
      (v.scopes ? `<br>权限：<span style="font-family:ui-monospace,monospace">${esc(v.scopes)}</span>` : "");
  } else if (v.status === "logged_out") {
    box.innerHTML =
      `未登录 <b>${esc(v.host || "github.com")}</b>。把下面这行粘到终端执行：` +
      `<br><code style="display:block;margin:6px 0;padding:6px 8px;background:var(--card-2);` +
      `border-radius:6px;font-family:ui-monospace,monospace;font-size:11.5px;user-select:all">` +
      `${esc(v.command || "")}</code>${esc(v.hint || "")}`;
  } else if (v.status === "gh_missing") {
    box.innerHTML =
      `${esc(v.hint || "未检测到 GitHub CLI")}<br>` +
      `安装：<code style="user-select:all">${esc(v.install || "")}</code>` +
      (v.fallback ? `<br><span style="color:var(--faint)">${esc(v.fallback)}</span>` : "");
  } else {
    box.textContent = v.hint || "状态未知";
  }
}

function initGitAuth() {
  const btn = $("git-auth-refresh");
  if (btn) {
    btn.onclick = async () => {
      await renderGitAuth();
      const f = $("git-auth-flash");
      if (f) { f.textContent = "已刷新"; setTimeout(() => { f.textContent = ""; }, 1600); }
    };
  }
  renderGitAuth();
}

async function loadConfig() {
  if (typeof initGitAuth === "function") initGitAuth(); // T-13：GitHub 登录态（独立请求，不拖慢主配置）
  try {
    await loadPresets(); // 站点清单就绪后再回显，base_url 反查预设
    const c = (await api("GET", "/api/config")).config;
    $("llm-provider").value = c.llm.provider;
    const model = c.llm.model ?? "";
    // P1：用户手动选过模型时，以面板当前值为准——自动同步不得把**未保存**的选择冲掉
    const keepPick = modelPicked ? curModelName() : "";
    const sel = $("llm-model");
    ensureCustomOption(sel); // P1：先备好「（自定义…）」选项——否则 sel.value 赋值静默失败，模型在面板上根本看不见
    if ([...sel.options].some((o) => o.value === model)) {
      sel.value = model; $("llm-model-custom").value = "";
    } else {
      sel.value = "__custom__"; $("llm-model-custom").value = model;
    }
    syncModelField();
    modelPicked = false; // 已按服务端已保存配置回显（后续自动同步以它为准）
    // P1：打开设置即按**服务端已保存**的 provider/base_url 同步模型目录
    // （网关 /v1/models 为单一事实源；此处不传 provider 覆盖，见 autoSyncModels 说明）
    lastSyncedModel = keepPick || model || "";
    autoSyncModels(lastSyncedModel);
    const isOllama = c.llm.provider === "ollama";
    $("llm-base").value = isOllama ? (c.llm.ollama_host ?? "localhost:11434") : (c.llm.base_url ?? "");
    $("llm-key").value = "";
    $("llm-key").placeholder = c.llm.key?.key_set ? `已设置（尾号 ${c.llm.key.key_tail}，留空不修改）` : "未设置";
    if (sitePresets.length && c.llm.provider === "openai") { $("llm-site").value = curSiteId(); updateSiteLink(curSiteId()); }
    // 媒体模型回显（tools PLAN §九 M3）：media 段 → 面板字段；key 只回掩码，留空 = 不修改
    const m = c.media ?? {};
    const mi = m.image ?? {}, mv = m.video ?? {};
    $("media-img-base").value = mi.base_url ?? "";
    $("media-img-model").value = mi.model ?? "";
    $("media-img-key").value = "";
    $("media-img-key").placeholder = mi.key?.key_set ? `已设置（尾号 ${mi.key.key_tail}，留空不修改）` : "未设置";
    $("media-vid-base").value = mv.base_url ?? "";
    $("media-vid-model").value = mv.model ?? "";
    $("media-vid-key").value = "";
    $("media-vid-key").placeholder = mv.key?.key_set ? `已设置（尾号 ${mv.key.key_tail}，留空不修改）` : "未设置";
    $("media-vid-submit").value = mv.submit_url ?? "";
    $("media-vid-query").value = mv.query_url ?? "";
    // W19 站点默认参数回显（对象 → JSON 串）
    $("media-vid-extra").value = mv.extra && typeof mv.extra === "object" ? JSON.stringify(mv.extra) : "";
    // 外部通道通用扩展点：整段回显为 JSON（敏感字段已是掩码），留空 = 未配置
    const chEl = $("channels-json");
    if (chEl) chEl.value = Object.keys(c.external_channels ?? {}).length ? JSON.stringify(c.external_channels, null, 2) : "";
    // 组头状态徽章（pcnt 位）：配置一眼可见
    $("llm-cnt").textContent = c.llm.model || c.llm.provider || "未配置";
    $("media-img-cnt").textContent = mi.base_url ? "已配置" : "未配置";
    $("media-vid-cnt").textContent = (mv.base_url && (mv.submit_url || mv.query_url)) ? "已配置" : "未配置";
    cfgTools = c.tools ?? [];
    cfgMcp = c.mcp ?? { servers: [], tools: [] };
    if (!$("tab-tools").hidden) renderTools(); // 工具 tab 可见时回填（覆盖 fetch 完成晚于 tab 切换的时序）
    // （MCP key 配置已移入「MCP 外接」各服务组内，随 renderTools 渲染）
    syncLlmFields();
    refreshUpstream(); // 回环 base_url 时亮出本地网关状态/积分（非回环自动隐藏）
    showInstBadge(c.instance_name ?? ""); // W17：子实例头部徽章（主实例为空不显示）
    // F3：缓存本轮 config 喂「能力总览」——总览是只读目录，不该为它多发一次 /api/config
    window.__cfgCache = c;
    renderOverview();
    const a = c.agent ?? {};
    for (const [id, v] of [
      ["agent-max-rounds", a.max_rounds], ["agent-output-dir", a.output_dir ?? "outputs"], ["agent-budget-secs", a.budget_secs], ["agent-token-budget", a.token_budget],
      ["agent-history-limit", a.history_limit], ["agent-compact-trigger", a.compact_trigger], ["agent-compact-keep", a.compact_keep],
      ["agent-tool-result-limit", a.tool_result_limit], ["agent-retry-attempts", a.retry_attempts], ["agent-retry-base-ms", a.retry_base_ms],
      ["agent-llm-context-tokens", a.llm_context_tokens],
    ]) $(id).value = v ?? "";
    $("agent-system-prompt").value = a.system_prompt ?? "";
    // IM 通道回显（external_channels.im）：凭据只回 key_set + key_tail，留空 = 不修改
    const im = c.external_channels?.im ?? {};
    for (const [el, v] of [
      ["im-enabled", im.enabled], ["im-enable-dingtalk", im.enable_dingtalk], ["im-enable-qq", im.enable_qq],
    ]) $(el).checked = !!v;
    for (const [el, v] of [
      ["im-host-base", im.host_base], ["im-dt-id", im.dingtalk_client_id], ["im-qq-appid", im.qq_appid],
      ["im-allow-users", im.allow_users], ["im-allow-chats", im.allow_chats],
      ["im-max-chars", im.max_reply_chars], ["im-timeout", im.chat_timeout_secs],
    ]) $(el).value = v ?? "";
    $("im-dt-secret").value = "";
    $("im-dt-secret").placeholder = im.dingtalk_client_secret?.key_set ? `已设置（尾号 ${im.dingtalk_client_secret.key_tail}，留空不修改）` : "未设置";
    $("im-qq-token").value = "";
    $("im-qq-token").placeholder = im.qq_token?.key_set ? `已设置（尾号 ${im.qq_token.key_tail}，留空不修改）` : "未设置";
  } catch (e) { flash("llm-flash", false, "加载失败: " + e.message); }
}

// ── W17 多窗口多实例：徽章 + 「新窗口」modal（免填实例名：自动取工作区文件夹名）──
function showInstBadge(name) {
  const b = $("inst-badge");
  if (name) { b.textContent = "▣ " + name; b.hidden = false; }
}
$("btn-newwin").onclick = () => {
  $("win-modal").hidden = false;
  $("win-flash").textContent = "";
  $("win-ws").value = "";
  renderInstList();
  $("win-browse").focus();
};
// ── W17 迭代十：按需复活——modal 内实例列表（在线打开 / 离线启动）──
// offline 实例「启动」= POST /api/instances 同工作区（create_instance 复活路径：
// 换新端口，memory/config/会话延续）；在线实例直接开 url 新标签。
async function renderInstList() {
  const box = $("inst-list");
  box.textContent = "加载中…";
  let list = [];
  try {
    const v = await api("GET", "/api/instances");
    list = v.instances || [];
  } catch (e) {
    box.textContent = "";
    return; // 清单不可用不阻断建窗主流程
  }
  box.textContent = "";
  if (!list.length) {
    box.innerHTML = '<span class="hint" style="margin:0">暂无注册实例（首次用下方创建）</span>';
    return;
  }
  for (const it of list) {
    const row = document.createElement("div");
    row.style.cssText = "display:flex; align-items:center; gap:8px; padding:6px 10px; border:1px solid var(--border,#ddd); border-radius:8px";
    const badge = it.online
      ? '<span style="color:#2e7d32; font-weight:600">● 在线</span>'
      : (it.stopped ? '<span style="color:#9e9e9e">○ 已手动停止</span>' : '<span style="color:#b26a45">○ 离线</span>');
    row.innerHTML =
      '<div style="flex:1; min-width:0"><div style="font-weight:600">' + esc(it.name) + "</div>" +
      '<div class="hint" style="margin:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap">' + esc(it.workspace || "") + "</div></div>" + badge;
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "btn ghost";
    btn.style.whiteSpace = "nowrap";
    btn.textContent = it.online ? "打开" : "启动";
    btn.onclick = async () => {
      btn.disabled = true;
      if (it.online) {
        window.open(it.url, "_blank");
        btn.disabled = false;
        return;
      }
      flash("win-flash", true, "启动 " + it.name + " 中（等待就绪，最多 ~15s）…");
      try {
        const v = await api("POST", "/api/instances", { workspace: it.workspace });
        flash("win-flash", true, "已启动 " + v.url + "（实例名 " + v.name + "）");
        window.open(v.url, "_blank");
        renderInstList();
      } catch (e) {
        flash("win-flash", false, e.message);
        btn.disabled = false;
      }
    };
    row.appendChild(btn);
    box.appendChild(row);
  }
}
$("win-browse").onclick = async () => {
  const btn = $("win-browse");
  btn.disabled = true;
  flash("win-flash", true, "请在弹出的文件夹选择框中选择…");
  try {
    const v = await api("POST", "/api/pick-folder");
    $("win-ws").value = v.path;
    flash("win-flash", true, "");
    $("win-create").focus();
  } catch (e) {
    flash("win-flash", false, e.message);
  } finally { btn.disabled = false; }
};
$("win-cancel").onclick = () => { $("win-modal").hidden = true; };
$("win-modal").onclick = (e) => { if (e.target === $("win-modal")) $("win-modal").hidden = true; };
$("win-create").onclick = async () => {
  const ws = $("win-ws").value.trim();
  if (!ws) { flash("win-flash", false, "请先选择工作区文件夹"); return; }
  // 同步先开空白标签占位（保住用户手势，避免 await 后被浏览器弹窗拦截）
  const winRef = window.open("about:blank", "_blank");
  const btn = $("win-create");
  btn.disabled = true;
  flash("win-flash", true, "创建中（实例名自动取文件夹名；等待新窗口就绪，最多 ~15s）…");
  try {
    const v = await api("POST", "/api/instances", { workspace: ws });
    flash("win-flash", true, (v.reused ? "已复用运行中窗口 " : "已创建 ") + v.url + "（实例名 " + v.name + "）");
    winRef.location.href = v.url;
    setTimeout(() => { $("win-modal").hidden = true; }, 800);
  } catch (e) {
    winRef.close();
    flash("win-flash", false, e.message);
  } finally { btn.disabled = false; }
};

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
// ── P1 模型目录自动同步 ──────────────────────────────────────────────────────
// 单一事实源：网关 /v1/models（= /api/models 不带 provider 覆盖的转发结果）。
// 三个坑（均为实测，不是推测）：
//  ① 自动同步**不带** ?provider= 覆盖——llm-adapter 用的是它自己进程里的 env（config.json 热应用），
//     带覆盖等于把 UI 里**尚未保存**的值当真值（实测 ?provider=ollama 返回本机 ollama 清单，
//     与网关的 90 个模型毫无关系）。
//  ② 当前模型不在新目录里时**继续显示为自定义值**，不得改成第一个模型——
//     静默换模型 = 用户下一次发言用了别的模型却不知道（且原模型可能已下线）。
//  ③ 自动失败必须**看得见 + 能重试**：静默失败等于目录永远停在旧快照，用户无从判断。
let lastSyncedModel = "";  // 最近一次按服务端已保存配置同步到的模型（重试时沿用）
let modelPicked = false;   // 用户在面板上手动选过模型（自动同步不得冲掉未保存的选择）
let syncing = false;       // 同步在飞标记（并发请求会把下拉框刷成两个来源的结果）
let syncPending = "";      // 在飞期间又来一次同步请求 → 结束后补跑（用最新模型值）
function ensureCustomOption(sel) {
  if (![...sel.options].some((o) => o.value === "__custom__")) {
    const oc = document.createElement("option");
    oc.value = "__custom__"; oc.textContent = "（自定义…）";
    sel.appendChild(oc);
  }
}
// 目录状态行：kind = ""（隐藏）/ "ok" / "err"；err 附「重试」按钮（重跑自动同步）
function modelCatalogStatus(kind, text) {
  const box = $("llm-model-sync");
  if (!box) return;
  if (!kind) { box.hidden = true; box.innerHTML = ""; return; }
  box.hidden = false;
  box.innerHTML = `<span class="flash ${kind === "ok" ? "ok" : "err"}">${esc(text)}</span>` +
    (kind === "err" ? ` <button type="button" class="btn ghost" id="llm-model-retry">重试</button>` : "");
  const rb = $("llm-model-retry");
  if (rb) rb.onclick = () => autoSyncModels(lastSyncedModel);
}
// 用目录重建下拉：cur 命中 → 选中；未命中 → 继续显示为自定义值（**不改成第一个模型**）
function applyModelCatalog(models, meta, cur) {
  const sel = $("llm-model");
  modelsMeta = Array.isArray(meta) ? meta : [];
  const ctxByName = new Map(modelsMeta.filter((m) => m.ctx_limit).map((m) => [m.name, m.ctx_limit]));
  // 一次性换选项：逐次 appendChild 会让 childList MutationObserver 收到 N 次回调，
  // 152 项目录同步时 rgRenderMissing 被反复触发。DocumentFragment 只产生 1 次 childList 变更。
  const frag = document.createDocumentFragment();
  for (const m of models) {
    const o = document.createElement("option");
    o.value = m;
    o.textContent = ctxByName.has(m) ? `${m} · ${fmtCtx(ctxByName.get(m))}` : m;
    frag.appendChild(o);
  }
  sel.replaceChildren(frag, ...(!models.includes("__custom__") ? [new Option("（自定义…）", "__custom__")] : []));
  const keep = (cur ?? "").trim();
  if (keep && models.includes(keep)) {
    sel.value = keep; $("llm-model-custom").value = "";
  } else if (keep) {
    sel.value = "__custom__"; $("llm-model-custom").value = keep;
  } else {
    sel.value = models[0] || "__custom__";
  }
  syncModelField();
  mpickRender(models, modelsMeta, keep);  // P7：常驻 chips + 平台分组折叠 + 搜索
}
// ── P7 模型选择器：常驻 chips + 平台分组折叠 + 搜索 ─────────────────────────────
// 三条既有契约（改前必读，都是上一轮实测换来的）：
//  ① 唯一真值仍是原生 <select id="llm-model">（现由 style.css 隐藏）：curModelName / rgCatalogHas /
//     MutationObserver / keepCurrentAsCustom / 保存流程全都读它——换成自定义 DOM 会牵动六处。
//  ② 「选一个模型」只做一件事：写 select.value 后派发 change。modelPicked、syncModelField、
//     updateCtxHint、rgRenderMissing 全挂在 change 上，这里不重复实现（否则两处判定会漂）。
//  ③ 当前模型不在目录里时选中态必须落到 __custom__（不得静默换成第一个，见 applyModelCatalog）。
const MPICK_PIN_KEY = "rsi.llm.pinnedModels";   // 常用模型（用户自己挑的那一框，跨刷新/重启常驻）
const MPICK_OPEN_KEY = "rsi.llm.openGroups";    // 平台分组的折叠态
const MPICK_PIN_MAX = 24;
const MPICK_ROUTE_GROUP = "auto（路由组）";  // 网关路由组单列一栏：owned_by 只是它默认落到的平台
let mpickState = { models: [], meta: new Map(), open: null };

function mpickStore(k) { try { return JSON.parse(localStorage.getItem(k) || "null"); } catch (e) { return null; } }
function mpickSave(k, v) { try { localStorage.setItem(k, JSON.stringify(v)); } catch (e) { /* 隐私模式写不进：只留内存态，不报错 */ } }
function mpickPins() {
  const v = mpickStore(MPICK_PIN_KEY);
  return Array.isArray(v) ? v.filter((x) => typeof x === "string") : [];
}
function mpickOpenState() {
  if (!mpickState.open) {
    const v = mpickStore(MPICK_OPEN_KEY);
    mpickState.open = v && typeof v === "object" ? v : {};
  }
  return mpickState.open;
}
function mpickPlatform(name) {
  const m = mpickState.meta.get(name) || {};
  // route 标记来自 adapter 的 models_meta；adapter 未重启（或站点无 meta）时按 id 前缀兜底，
  // 保证「路由组」栏位在降级态也不与平台栏混在一起。
  if (m.route === "auto" || (!m.platform && name.indexOf("auto/") === 0)) return MPICK_ROUTE_GROUP;
  return m.platform || (name.indexOf("/") > 0 ? name.split("/")[0] : "其他");
}
function mpickSelect(name) {
  const sel = $("llm-model");
  ensureCustomOption(sel);
  if ([...sel.options].some((o) => o.value === name)) { sel.value = name; $("llm-model-custom").value = ""; }
  else { sel.value = "__custom__"; $("llm-model-custom").value = name; }
  // 关键：把 lastSyncedModel 同步到用户选择。后台 autoSyncModels() 用它作 keep 回写
  // select.value；不同步就会在任意一次自动同步后把未保存的选择静默改回服务端旧模型。
  lastSyncedModel = curModelName();
  syncModelField();
  updateCtxHint();
  sel.dispatchEvent(new Event("change"));  // modelPicked / rgRenderMissing / 高亮重画都挂在这上面
}
function mpickPaintCur() {
  const el = $("llm-model-cur");
  if (!el) return;
  const n = curModelName();
  if (!n) { el.className = "mpick-cur"; el.textContent = "未选择"; return; }
  const m = mpickState.meta.get(n) || {};
  const miss = mpickState.models.length > 0 && !mpickState.models.includes(n);
  const tags = [m.platform || "", m.ctx_limit ? fmtCtx(m.ctx_limit) : "", m.route === "auto" ? "路由组" : ""]
    .filter(Boolean).join(" · ");
  el.className = "mpick-cur" + (miss ? " warn" : "");
  el.innerHTML = `<b>${esc(n)}</b>`
    + (tags ? ` <span class="mpick-tag">${esc(tags)}</span>` : "")
    + (miss ? ` <span class="mpick-tag">不在当前目录</span>` : "");
}
function mpickPaintPins() {
  const box = $("mpick-pins");
  if (!box) return;
  const pins = mpickPins();
  const cur = curModelName();
  if (!pins.length) {
    box.innerHTML = `<span class="hint" style="margin:0">还没有常用模型——在下面「全部模型」里点 ☆ 加入（存在本机浏览器）</span>`;
    return;
  }
  box.innerHTML = pins.map((n) => {
    const miss = mpickState.models.length > 0 && !mpickState.models.includes(n);
    const title = (mpickState.meta.get(n) || {}).display_name || n;
    return `<span class="chip${n === cur ? " on" : ""}${miss ? " miss" : ""}" data-pin-pick="${escAttr(n)}"`
      + ` title="${escAttr(title + (miss ? "（该模型已不在当前目录）" : ""))}">${esc(n)}`
      + `<button type="button" class="chip-x" data-unpin="${escAttr(n)}" title="移出常用">✕</button></span>`;
  }).join("");
}
function mpickPaintGroups() {
  const box = $("mpick-all");
  if (!box) return;
  const q = (($("mpick-q") || {}).value || "").trim().toLowerCase();
  const open = mpickOpenState();
  const cur = curModelName();
  const pins = mpickPins();
  const groups = new Map();
  for (const n of mpickState.models) {
    const p = mpickPlatform(n);
    if (!groups.has(p)) groups.set(p, []);
    groups.get(p).push(n);
  }
  // 平台顺序：路由组置顶（网关特有的选择面），其余按模型数量降序——常用平台先看到
  const keys = [...groups.keys()].sort((a, b) => {
    if (a === MPICK_ROUTE_GROUP || b === MPICK_ROUTE_GROUP) return a === MPICK_ROUTE_GROUP ? -1 : 1;
    return groups.get(b).length - groups.get(a).length || a.localeCompare(b);
  });
  const hit = (n) => {
    if (!q) return true;
    const m = mpickState.meta.get(n) || {};
    return n.toLowerCase().includes(q)
      || mpickPlatform(n).toLowerCase().includes(q)
      || String(m.display_name || "").toLowerCase().includes(q);
  };
  const parts = [];
  let shown = 0;
  for (const p of keys) {
    const names = groups.get(p).filter(hit);
    if (!names.length) continue;
    shown += names.length;
    // 折叠默认：有搜索词全展开（否则搜到了还得点开一层）；平台少（≤4）全展开；
    // 其余只展开路由组与「当前模型所在平台」——152 项一次摊开正是这轮要解决的乱
    const isOpen = q ? true
      : (p in open ? !!open[p] : (keys.length <= 4 || p === MPICK_ROUTE_GROUP || names.indexOf(cur) >= 0));
    parts.push(`<div class="mpick-group${isOpen ? "" : " shut"}" data-grp="${escAttr(p)}">`
      + `<div class="mpick-gh"><span class="chev">${isOpen ? "▾" : "▸"}</span>`
      + `<span class="mpick-gn">${esc(p)}</span><span class="mpick-gc">${names.length}</span></div>`
      + `<div class="mpick-gb"${isOpen ? "" : " hidden"}>`
      + names.map((n) => {
          const m = mpickState.meta.get(n) || {};
          const pinned = pins.indexOf(n) >= 0;
          const extra = [m.ctx_limit ? fmtCtx(m.ctx_limit) : "", m.route === "auto" ? "auto" : ""].filter(Boolean).join(" · ");
          return `<div class="mpick-item${n === cur ? " on" : ""}" data-pick="${escAttr(n)}">`
            + `<span class="mpick-nm">${esc(n)}</span>`
            + (m.display_name && m.display_name !== n ? `<span class="mpick-dn">${esc(m.display_name)}</span>` : "")
            + (extra ? `<span class="mpick-tag">${esc(extra)}</span>` : "")
            + `<button type="button" class="mpick-star${pinned ? " on" : ""}" data-pin="${escAttr(n)}"`
            + ` title="${pinned ? "移出常用" : "加入常用"}">${pinned ? "★" : "☆"}</button></div>`;
        }).join("")
      + `</div></div>`);
  }
  box.innerHTML = parts.length
    ? (q ? `<p class="hint" style="margin:0 0 6px">匹配 ${shown} / ${mpickState.models.length} 个</p>` : "") + parts.join("")
    : `<p class="hint" style="margin:0">没有匹配的模型${q ? `（关键词「${esc(q)}」）` : "：目录为空，点上方「拉取模型」"}</p>`;
}
function mpickRender(models, meta, cur) {
  mpickState.models = Array.isArray(models) ? models : [];
  mpickState.meta = new Map((Array.isArray(meta) ? meta : [])
    .filter((m) => m && typeof m.name === "string")
    .map((m) => [m.name, m]));
  mpickPaintCur();
  mpickPaintPins();
  mpickPaintGroups();
}
// 只切「当前选中」高亮，不碰 DOM 结构：change 事件高频（每次选模型/自动同步回显都触发），
// 全量重建 152 项目录树是设置页卡顿的主因。结构只在 mpickRender（目录真变了）时重建。
function mpickHighlight() {
  const cur = curModelName();
  const box = $("mpick-all");
  if (box) for (const it of box.querySelectorAll(".mpick-item")) {
    it.classList.toggle("on", it.dataset.pick === cur);
  }
  const pins = $("mpick-pins");
  if (pins) for (const c of pins.querySelectorAll(".chip")) {
    c.classList.toggle("on", c.dataset.pinPick === cur);
  }
}
(function bindMpick() {
  const all = $("mpick-all"), pins = $("mpick-pins");
  if (!all || !pins) return;  // 老页面（未带 P7 结构）直接跳过，不影响既有下拉
  const q = $("mpick-q");
  // 搜索输入防抖：连续敲键只在停手 120ms 后重绘一次分组，避免每个字符都重建目录树
  if (q) {
    let qTimer = 0;
    q.addEventListener("input", () => { clearTimeout(qTimer); qTimer = setTimeout(mpickPaintGroups, 120); });
  }
  pins.addEventListener("click", (e) => {
    const x = e.target.closest("[data-unpin]");
    if (x) {   // 移出：点 ✕ 不触发选中
      e.stopPropagation();
      mpickSave(MPICK_PIN_KEY, mpickPins().filter((n) => n !== x.dataset.unpin).slice(0, MPICK_PIN_MAX));
      mpickPaintPins(); mpickPaintGroups();
      return;
    }
    const c = e.target.closest("[data-pin-pick]");
    if (c) mpickSelect(c.dataset.pinPick);
  });
  all.addEventListener("click", (e) => {
    const st = e.target.closest("[data-pin]");
    if (st) {  // 星标开合：不能顺带把模型切了
      e.stopPropagation();
      const n = st.dataset.pin;
      const cur = mpickPins();
      const next = cur.indexOf(n) >= 0 ? cur.filter((x) => x !== n) : cur.concat([n]).slice(0, MPICK_PIN_MAX);
      mpickSave(MPICK_PIN_KEY, next);
      mpickPaintPins(); mpickPaintGroups();
      return;
    }
    const gh = e.target.closest(".mpick-gh");
    if (gh) {
      const g = gh.parentElement, body = g.querySelector(".mpick-gb");
      const nowOpen = body.hidden;
      body.hidden = !nowOpen;
      mpickOpenState()[g.dataset.grp] = nowOpen;
      mpickSave(MPICK_OPEN_KEY, mpickOpenState());
      gh.querySelector(".chev").textContent = nowOpen ? "▾" : "▸";
      g.classList.toggle("shut", !nowOpen);
      return;
    }
    const it = e.target.closest("[data-pick]");
    if (it) mpickSelect(it.dataset.pick);
  });
  // 模型从别处变化（自动同步、保存回显、自定义输入）→ 只切高亮，目录结构不动
  $("llm-model").addEventListener("change", () => { mpickPaintCur(); mpickHighlight(); });
  $("llm-model-custom").addEventListener("input", mpickPaintCur);
  mpickRender([], [], "");
})();
async function fetchModels(silent) {
  const btn = $("llm-fetch-models");
  const prev = btn.textContent;
  if (!silent) { btn.disabled = true; btn.textContent = "拉取中…"; }
  try {
    // 手动「拉取模型」按 UI 里的 provider 取（用户可能正在试一个**尚未保存**的站点）；
    // 自动同步不走这条路（见 autoSyncModels 的说明 ①）。
    const v = await api("GET", "/api/models?provider=" + encodeURIComponent($("llm-provider").value));
    if (v && v.ok && Array.isArray(v.models) && v.models.length) {
      // 当前模型不在新目录里时同样保留为自定义值——否则「拉取模型」会顺手把用户选的模型
      // 换成第一个，用户再点保存就悄悄换掉了模型。
      applyModelCatalog(v.models, v.models_meta, curModelName());
      if (!silent) flash("llm-flash", true, `已拉取 ${v.models.length} 个模型：在下方「全部模型」里挑，或点 ☆ 加进「常用模型」`);
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
// 自动同步：按**服务端已保存**的 provider/base_url 拉目录（不带 provider 覆盖）
async function autoSyncModels(savedModel) {
  const cur = (savedModel ?? lastSyncedModel ?? "").trim();
  if (cur) lastSyncedModel = cur;
  if (syncing) { syncPending = lastSyncedModel; return false; }  // 别并发发第二次（两源相冲）
  syncing = true;
  modelCatalogStatus("", "");
  try {
    const v = await api("GET", "/api/models");
    if (v && v.ok && Array.isArray(v.models) && v.models.length) {
      applyModelCatalog(v.models, v.models_meta, lastSyncedModel);
      modelCatalogStatus("ok", `已按已保存配置同步 ${v.models.length} 个模型`);
      return true;
    }
    modelCatalogStatus("err", "模型目录为空：" + (v?.error?.message || "网关未返回 /v1/models 清单"));
    keepCurrentAsCustom(lastSyncedModel);
    return false;
  } catch (e) {
    modelCatalogStatus("err", "自动同步模型目录失败：" + e.message);
    keepCurrentAsCustom(lastSyncedModel);
    return false;
  } finally {
    syncing = false;
    updateCtxHint();
    if (syncPending) { const p = syncPending; syncPending = ""; autoSyncModels(p); }
  }
}
// 失败兜底：当前模型继续以自定义值显示（不丢、不静默换成第一个）
function keepCurrentAsCustom(cur) {
  const keep = (cur ?? "").trim();
  if (!keep) return;
  const sel = $("llm-model");
  ensureCustomOption(sel);
  if (![...sel.options].some((o) => o.value === keep)) {
    sel.value = "__custom__"; $("llm-model-custom").value = keep;
    syncModelField();
  }
}
// 手动改动模型 → 记为「用户选过」，自动同步据此保留未保存的选择
$("llm-model").addEventListener("change", () => { modelPicked = true; });
$("llm-model-custom").addEventListener("input", () => { modelPicked = true; lastSyncedModel = curModelName(); });
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
    if (saved) {
      $("llm-key").value = saved;
      $("llm-key").placeholder = `已记忆该站点 key（尾号 ${saved.slice(-4)}，可覆盖）`;
    } else if (p.key_hint) {
      $("llm-key").value = p.key_hint; // 免 key 站点：直接预填提示值（如 magpie 本地网关任意令牌）
      $("llm-key").placeholder = p.key_hint;
    } else {
      $("llm-key").value = "";
      $("llm-key").placeholder = "未保存过该站点 key";
    }
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
$("llm-site").addEventListener("change", () => { applySite($("llm-site").value); refreshUpstream(); });
// ── 上游本地网关（magpie）：状态灯 + 一键启动 + 积分行 ──
// base_url 为本机回环时显示网关块；GET /api/upstream/status（探活 + /magpie/quotas），
// POST /api/upstream/start 幂等 spawn magpie serve。非 magpie 站点无 quotas 端点 → 积分行自然隐藏。
async function refreshUpstream() {
  const base = $("llm-base").value.trim();
  const local = /^https?:\/\/(127\.0\.0\.1|localhost|::1|\[::1\])(:\d+)?/i.test(base);
  const field = $("upstream-field"), quotas = $("upstream-quotas");
  if (!local) {
    field.hidden = true; quotas.hidden = true;
    const ck = $("upstream-checkin-field"); if (ck) ck.hidden = true;
    return;
  }
  field.hidden = false;
  const ckf = $("upstream-checkin-field"); if (ckf) ckf.hidden = false;
  refreshCheckinAccounts();
  const st = $("upstream-state");
  try {
    renderUpstream(await api("GET", "/api/upstream/status"));
  } catch (e) {
    st.textContent = "状态获取失败"; st.className = "up-state down";
    $("upstream-start").hidden = false; quotas.hidden = true;
  }
}
function renderUpstream(v) {
  const st = $("upstream-state"), q = $("upstream-quotas");
  st.textContent = v.up ? `运行中 · ${v.models ?? "?"} 个模型` : "未运行";
  st.className = "up-state " + (v.up ? "up" : "down");
  $("upstream-start").hidden = !!v.up;
  // 积分行：每账户一行（provider 名 + user + 各窗口剩余百分比/额度/重置日）
  const rows = [];
  for (const acc of (v.quotas?.data ?? [])) {
    const wins = (acc.windows ?? []).map((w) => {
      const reset = w.resetsAt ? `，${w.resetsAt.slice(5, 10)} 重置` : "";
      const pct = (w.limit > 0 && typeof w.amount === "number") ? Math.round(((w.limit - w.amount) / w.limit) * 100)
        : ((w.remaining ?? 0) + (w.used ?? 0) > 0 ? Math.round(((w.remaining ?? 0) / ((w.remaining ?? 0) + (w.used ?? 0))) * 100) : null);
      return pct == null ? `${w.name} ${w.display ?? ""}` : `${w.name} 剩 ${pct}%（${w.display ?? ""}${reset}）`;
    }).join("；");
    if (wins) rows.push(`<b>${esc(acc.name ?? acc.provider ?? "")}</b>（${esc(acc.user ?? "")}）：${esc(wins)}`);
  }
  if (rows.length) { q.innerHTML = rows.join("<br>"); q.hidden = false; } else q.hidden = true;
}
// ── 账号签到（magpie 内置渠道）：登录弹窗 / 一键领取 / 状态行 ──
// 登录渠道**动态**渲染：GET /api/upstream/providers（Host 侧来自 `magpie plugin list --json`）。
// 为什么不在前端写死渠道表：渠道是插件能力，装了什么就有什么；写死必然与用户机器漂移
// （曾出现「MiniMax 登录入口」在插件里根本不存在），也会漏掉新装插件。
let checkinProvidersLoaded = false;
async function refreshCheckinProviders(force) {
  const box = $("upstream-login-btns");
  if (!box) return;
  if (checkinProvidersLoaded && !force) return;
  try {
    const v = await api("GET", "/api/upstream/providers");
    const list = Array.isArray(v.providers) ? v.providers : [];
    box.textContent = "";
    for (const p of list) {
      const label = p.name || p.id;
      const b = document.createElement("button");
      b.type = "button";
      b.className = "btn ghost";
      if (p.usable) {
        b.textContent = `登录 ${label} 账号`;
        b.onclick = () => loginProvider(p.id, label, b);
      } else {
        // 该渠道的 method 需要交互输入（api key / 企业域名），Host 子进程 stdin=null 做不了
        b.textContent = `登录 ${label} 账号（需终端）`;
        b.disabled = true;
        b.title = `${label} 的登录方式需要交互输入，请在终端执行：magpie plugin login ${p.id}`;
      }
      box.appendChild(b);
    }
    if (!list.length) box.textContent = "未发现可登录渠道（检查 magpie 插件是否安装）";
    checkinProvidersLoaded = true;
  } catch { checkinProvidersLoaded = false; }
}
async function loginProvider(pid, label, btn) {
  btn.disabled = true; btn.textContent = `${label} 登录中…（在弹窗完成登录后自动恢复）`;
  flash("llm-flash", true, `${label} 登录窗已弹出——在窗口里完成验证，关窗后自动刷新账号。`);
  try { await api("POST", `/api/upstream/login?provider=${encodeURIComponent(pid)}`); flash("llm-flash", true, `${label} 登录流程结束，正在刷新账号…`); }
  catch (e) { flash("llm-flash", false, `${label} 登录流程提前退出：${e.message}（若窗口里已完成登录，稍后自动刷新即可确认）`); }
  btn.disabled = false; btn.textContent = `登录 ${label} 账号`;
  refreshCheckinAccounts(); refreshUpstream();
}
function renderCheckinState(text, cls) {
  const st = $("checkin-state");
  st.textContent = text;
  st.className = "up-state " + (cls ?? "");
}
async function refreshCheckinAccounts() {
  const field = $("upstream-checkin-field");
  if (field.hidden) return;
  refreshCheckinProviders();
  try {
    const v = await api("GET", "/api/upstream/accounts");
    const list = Array.isArray(v.accounts) ? v.accounts : (v.accounts?.accounts ?? []);
      const PN = { workbuddy: "WorkBuddy", "trae-cn": "Trae CN", qoder: "Qoder", kiro: "Kiro" };
      const f = x => (typeof x === "number" ? Math.round(x * 10) / 10 : x);
      const log = $("upstream-checkin-log");
      log.style.whiteSpace = "pre-wrap";
      log.textContent = list.map(a => {
        const name = `${PN[a.agent] ?? a.agent} ${a.user}${a.active ? "" : "（未激活）"}`;
        const qs = (a.windows ?? []).map(w => `${w.name ?? "积分"} 剩 ${f(w.remaining) ?? "?"}`).join("，");
        return `· ${name}：${qs || "暂无积分数据"}`;
      }).join("\n");
      log.hidden = list.length === 0;
    const withQuota = list.filter(a => (a.windows?.length ?? 0) > 0);
    renderCheckinState(`已登录 ${list.length} 个账号${withQuota.length ? `（${withQuota.length} 个有积分）` : ""}`, "up");
  } catch { renderCheckinState("账号状态获取失败", "down"); }
}
$("upstream-checkin").onclick = async () => {
  const btn = $("upstream-checkin");
  btn.disabled = true; btn.textContent = "领取中…";
  renderCheckinState("正在为全部账号签到…", "up");
  try {
    const v = await api("POST", "/api/upstream/checkin");
    const log = $("upstream-checkin-log");
    log.textContent = (v.lines ?? []).join("\n") || "（无输出）";
    log.hidden = false;
    flash("llm-flash", true, v.ok ? "签到完成（幂等：当天重复点只显示 already）" : "签到命令退出非 0，详情见结果行");
  } catch (e) { flash("llm-flash", false, `签到失败：${e.message}`); renderCheckinState("签到失败", "down"); }
  btn.disabled = false; btn.textContent = "一键领取积分";
  refreshCheckinAccounts(); refreshUpstream();
};
$("upstream-start").onclick = async () => {
  const btn = $("upstream-start");
  btn.disabled = true; btn.textContent = "启动中…";
  try {
    const v = await api("POST", "/api/upstream/start");
    if (!v.up) flash("llm-flash", false, "magpie serve 已拉起但未在 10s 内就绪（详情见 config 同目录 .magpie-serve.log）");
    else flash("llm-flash", true, "本地网关已就绪");
  } catch (e) { flash("llm-flash", false, e.message); }
  btn.disabled = false; btn.textContent = "一键启动";
  refreshUpstream();
};
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
    // 保存后由 loadConfig 内的自动同步刷新目录（走**已保存**配置）。
    // 原来这里另发一次带 provider 覆盖的 fetchModels()——两条路径并发会把下拉框
    // 刷成两个不同来源的结果，且覆盖路径用的是 UI 值而非服务端已保存值。
    await loadConfig();
  } catch (e) { flash("llm-flash", false, e.message); }
};
// ── 媒体模型独立保存（tools PLAN §九 M3）：生图/生视频各自 PUT media 段（逐字段 merge，
// key 留空不提交 = 不修改；后端 merge 对 null/空串不覆盖，故想清空某字段须整段重填）──
function mediaBody(kind) {
  const sec = kind === "image"
    ? { base_url: "media-img-base", model: "media-img-model" }
    : { base_url: "media-vid-base", model: "media-vid-model", submit_url: "media-vid-submit", query_url: "media-vid-query" };
  const out = {};
  for (const [field, id] of Object.entries(sec)) {
    const v = $(id).value.trim();
    if (v) out[field] = v;
  }
  const key = $(kind === "image" ? "media-img-key" : "media-vid-key").value.trim();
  if (key) out.key = key; // media 段字段名与 llm 段不同：是 key 不是 api_key
  // W19 站点默认参数：JSON 输入（非法即报错不提交；填 {} = 清空全部默认参数）
  if (kind === "video") {
    const raw = $("media-vid-extra").value.trim();
    if (raw) {
      try { out.extra = JSON.parse(raw); }
      catch { throw new Error("站点默认参数 extra 不是合法 JSON"); }
    }
  }
  return out;
}
async function saveMedia(kind, flashId) {
  let fields;
  try {
    fields = mediaBody(kind);
  } catch (e) { flash(flashId, false, e.message); return; }
  if (!Object.keys(fields).length) { flash(flashId, false, "未填写任何字段"); return; }
  try {
    await api("PUT", "/api/config", { media: { [kind]: fields } });
    flash(flashId, true, "已保存落盘（重启 host 后 env 生效）");
    loadConfig();
  } catch (e) { flash(flashId, false, e.message); }
}
$("media-img-save").onclick = () => saveMedia("image", "media-img-flash");
$("media-vid-save").onclick = () => saveMedia("video", "media-vid-flash");
// ── IM 通道配置保存： PUT external_channels.im，密码留空不提交（后端空串不改）──
$("im-save").onclick = async () => {
  const im = {};
  for (const [key, el] of Object.entries({
    enabled: "im-enabled", enable_dingtalk: "im-enable-dingtalk", enable_qq: "im-enable-qq",
  })) im[key] = $(el).checked;
  for (const [key, el] of Object.entries({
    host_base: "im-host-base", dingtalk_client_id: "im-dt-id", qq_appid: "im-qq-appid",
    allow_users: "im-allow-users", allow_chats: "im-allow-chats",
  })) { const v = $(el).value.trim(); if (v) im[key] = v; }
  for (const [key, el] of Object.entries({
    max_reply_chars: "im-max-chars", chat_timeout_secs: "im-timeout",
  })) { const v = $(el).value.trim(); if (v) im[key] = Number(v); }
  const secret = $("im-dt-secret").value.trim();
  if (secret) im.dingtalk_client_secret = secret;   // 留空 = 不修改
  const token = $("im-qq-token").value.trim();
  if (token) im.qq_token = token;                    // 留空 = 不修改
  try {
    await api("PUT", "/api/config", { external_channels: { im } });
    flash("im-flash", true, "已保存落盘（启动 im-gateway 后生效）");
    loadConfig();
  } catch (e) { flash("im-flash", false, e.message); }
};
// LLM tab 三区折叠（与工具池 poolhead 同款交互；静态 HTML，加载时绑定一次）
document.querySelectorAll("#tab-llm .poolhead.collapsible").forEach((head) => {
  const body = head.nextElementSibling;
  head.onclick = () => {
    body.hidden = !body.hidden;
    head.querySelector(".chev").textContent = body.hidden ? "▸" : "▾";
    head.classList.toggle("closed", body.hidden);
  };
});
// ── P2-D 路由组（auto 组）管理 ───────────────────────────────────────────────
// 为什么前端不自己拼 CLI：组名与模型 id 里带空格/引号/冒号（`kilo/recursive/flint-v1.0:free`），
// 直接拼 shell 就是命令注入面。这里只调 Host 的 /api/routegroups*，Host 用
// Command::args 传参（无 shell 解析）。
//
// 三个界面事实（均为实测，不是推测）：
//  ① 组有两套 id：内部 id（`auto-minimax-m3-1-flash`，规范化）与 public id
//     （`auto/minimax-m3.1-flash`，保留原始大小写/下划线/点号）。public 尾段**不能**
//     由内部 id 无损反推（`Doubao_1_6` 保留下划线，`Doubao-Seed-2.1-Turbo` 被小写），
//     所以列表把两者都显示出来，删除/恢复一律按内部 id 精确指向——裸 id 可能命中
//     内置 found 组并把它藏掉。
//  ② 用户组删除是真删除（不可恢复）；内置 found 组删除后留 hidden 记录，可 restore，
//     权威 restore id 由 `group show` 后缀给出（Host 已解析进 `restore_id`）。
//  ③ 模型目录里的 `auto/<model>` 只有在组**真实存在且可见**时才调得通。用户选了不存在
//     的 auto 模型时，界面必须当场说清「组不存在 + 能建什么 + 一键创建」，而不是等到
//     对话时才报网关错误。
let rgState = { groups: [], options: null, loaded: false, loading: false, pending: false, editing: "" };

function rgFlash(ok, msg) { flash("rg-flash", ok, msg); }

async function rgLoad() {
  const box = $("rg-list");
  // 在飞期间的第二次 rgLoad 不能直接丢：建/改/删/恢复组之后紧跟的那次刷新，正是「把新
  // 状态读回来」的那一次；丢掉它界面就停在旧列表（实测：flash 说建组成功，列表里却没有
  // 新组，用户以为没建成）。改成排队，由 finally 补跑一次。
  if (!box) return;
  if (rgState.loading) { rgState.pending = true; return; }
  rgState.loading = true;
  try {
    const [list, opts] = await Promise.all([
      api("GET", "/api/routegroups"),
      rgState.options ? Promise.resolve(rgState.options) : api("GET", "/api/routegroups/options"),
    ]);
    rgState.options = opts;
    rgState.groups = Array.isArray(list.groups) ? list.groups : [];
    rgState.loaded = true;
    rgFillOptions();
    rgRender();
    const c = list.counts || {};
    $("rg-cnt").textContent = `${c.visible ?? 0} 组${c.removed ? `（${c.removed} 个已移除）` : ""}`;
  } catch (e) {
    box.innerHTML = `<p class="flash err">读取路由组失败：${esc(e.message)}</p>`;
  } finally {
    rgState.loading = false;
    // 排队的那次补跑：放在 finally 里，成功/失败都补——失败时用户点「重试」也一样能生效。
    if (rgState.pending) { rgState.pending = false; rgLoad(); }
  }
}

// 下拉只在首次填充（用户可能已经点选过，重填会丢选择）。
function rgFillOptions() {
  const o = rgState.options;
  if (!o) return;
  const rsel = $("rg-routing"), ssel = $("rg-stays");
  if (!rsel.options.length) {
    rsel.innerHTML = (o.routings || []).map((r) => `<option value="${escAttr(r)}">${esc(r)}</option>`).join("");
    rsel.value = "smart";
  }
  if (!ssel.options.length) {
    ssel.innerHTML = (o.stays || []).map((s) => `<option value="${escAttr(s)}">${esc(s)}</option>`).join("");
    ssel.value = "auto";
  }
  const sel = $("rg-members");
  if (sel.options.length) return;
  sel.innerHTML = (o.models || []).map((m) => `<option value="${escAttr(m)}">${esc(m)}</option>`).join("");
}

function rgRender() {
  const box = $("rg-list");
  const gs = rgState.groups;
  if (!gs.length) {
    box.innerHTML = `<p class="hint">Magpie 里还没有任何路由组。下面填组名与成员即可新建。</p>`;
  } else {
    box.innerHTML = gs.map((g) => {
      const badges = [
        g.found ? `<span class="badge">内置</span>` : `<span class="badge">自建</span>`,
        g.visible ? "" : `<span class="badge">已移除</span>`,
      ].join(" ");
      const members = (g.members || [])
        .map((m) => `<span class="rg-mem" title="${escAttr(m.provider || "")}">${esc(m.id)}${m.selected ? " ✓" : ""}</span>`)
        .join(" ");
      const acts = [];
      if (g.visible) {
        acts.push(`<button type="button" class="btn ghost" data-rg-act="edit" data-rg-id="${escAttr(g.internal_id)}">编辑</button>`);
        acts.push(`<button type="button" class="btn ghost" data-rg-act="rm" data-rg-id="${escAttr(g.internal_id)}">删除</button>`);
      } else if (g.restorable) {
        acts.push(`<button type="button" class="btn ghost" data-rg-act="restore" data-rg-id="${escAttr(g.restore_id || g.internal_id)}">恢复</button>`);
      }
      const note = g.routing_note ? `<div class="hint" style="margin:2px 0 0;color:#c66">${esc(g.routing_note)}</div>` : "";
      return `<div class="ov" style="margin-bottom:8px">
        <div style="display:flex;gap:8px;align-items:baseline;flex-wrap:wrap">
          <b>${esc(g.name)}</b>${badges}
          <span class="hint" style="margin:0">public <code>${esc(g.public_id)}</code></span>
          <span class="hint" style="margin:0">内部 <code>${esc(g.internal_id)}</code></span>
          <span class="hint" style="margin:0">${esc(g.routing)}${g.stays ? ` / ${esc(g.stays)}` : ""}</span>
          <span style="margin-left:auto">${acts.join(" ")}</span>
        </div>
        <div class="hint" style="margin:4px 0 0">成员：${members || "（无）"}</div>
        ${note}
      </div>`;
    }).join("");
  }
  rgRenderMissing();
}

// 当前模型是否在最近一次同步到的目录里（下拉 option 就是那份目录）。
function rgCatalogHas(m) {
  return [...$("llm-model").options].some((o) => o.value === m);
}

// 模型下拉里选了 auto/<model> 但网关没这个组 → 当场提示 + 一键创建。
// 判定口径与 Host 一致：public id **精确等于**所选模型名（不做 slug 宽松匹配）。
function rgRenderMissing() {
  const box = $("rg-missing");
  if (!box) return;
  const hide = () => { box.hidden = true; box.innerHTML = ""; };
  const cur = curModelName();
  if (!cur.startsWith("auto/")) return hide();
  const tail = cur.slice(5);
  if (rgState.groups.some((g) => g.visible && g.public_id === cur)) return hide();
  // 还没读过组列表：目录里有它 = 上次同步时组还在，先不喊；目录里没有（自定义输入）
  // = 必须读一次才能判断，否则会在没证据的情况下报「组不存在」。
  if (!rgState.loaded) {
    if (!rgCatalogHas(cur)) rgLoad();
    return hide();
  }
  const others = rgState.groups.filter((g) => g.visible && g.public_id.startsWith("auto/")).map((g) => g.public_id);
  box.hidden = false;
  box.innerHTML = `<div class="ov-title">这个 auto 组不存在</div>
    <p class="hint" style="margin:0 0 6px">网关里没有 <code>${esc(cur)}</code>，直接发消息会失败。可以一键按模型尾段 <code>${esc(tail)}</code> 建组（成员取目录里尾段同名的真实模型）。</p>
    <div class="row" style="flex-wrap:wrap">
      <button type="button" class="btn" id="rg-missing-create">按 ${esc(tail)} 建组</button>
      ${others.length ? `<span class="hint" style="margin:0">现有 auto 组：${others.map((p) => `<code>${esc(p)}</code>`).join("、")}</span>` : ""}
    </div>`;
  $("rg-missing-create").onclick = () => rgQuickCreate(tail);
}

// 成员归并：同一真实模型在各平台常是不同 id（glm-5.3-flash / gfmodel / gmodel…），
// 但网关 /v1/models 的 display_name 相同。优先按 display_name 把别名一起纳入组，
// 组才能覆盖全部平台并自动故障转移；meta 缺失时回退到尾段匹配（老行为）。
//
// 例：target=glm-5.3-flash → 尾段命中 workbuddy/glm-5.3-flash，其 display_name=GLM-5.3-Flash；
// 再把所有 display_name=GLM-5.3-Flash 的模型（含 qoder/gfmodel）并入。
function rgMembersForTarget(tail) {
  const o = rgState.options || {};
  const models = o.models || [];
  const meta = Array.isArray(o.models_meta) ? o.models_meta : [];
  const tailOf = (s) => String(s || "").replace(/:[a-z+]+$/, "").split("/").pop();
  if (meta.length) {
    const nameOf = new Map(meta.map((m) => [m.name, (m.display_name || "").trim()]));
    // 1) target 尾段命中的模型 → 它们的 display_name 就是本组要归并的真实模型名
    const targets = new Set();
    for (const m of models) {
      if (tailOf(m) === tail && nameOf.get(m)) targets.add(nameOf.get(m));
    }
    // 2) 所有 display_name 命中的真实模型（含别名）都是成员
    if (targets.size) {
      const picked = models.filter((m) => targets.has(nameOf.get(m)));
      if (picked.length) return picked;
    }
  }
  // 回退：尾段匹配（含 effort 后缀归一）
  return models.filter((m) => tailOf(m) === tail);
}

async function rgQuickCreate(tail) {
  const members = rgMembersForTarget(tail);
  if (!members.length) {
    rgFlash(false, `网关目录里没有尾段为 ${tail} 的模型，无法自动建组——请先在 Magpie 侧暴露该模型`);
    return;
  }
  try {
    const v = await api("POST", "/api/routegroups", { name: tail, members, routing: "smart", stays: "auto", auto_model: tail });
    rgFlash(true, `已建组 ${(v.group && v.group.public_id) || tail}（成员 ${members.length} 个）`);
    await rgLoad();
    autoSyncModels(lastSyncedModel);  // 组变了 → auto/* 条目跟着变
  } catch (e) { rgFlash(false, "建组失败：" + e.message); }
}

// 编辑：把当前组载入表单（id 不可改——改 id 等于换一个组，应该新建）。
function rgEdit(internal) {
  const g = rgState.groups.find((x) => x.internal_id === internal);
  if (!g) return;
  rgState.editing = internal;
  $("rg-form-title").textContent = `编辑 ${g.public_id}（内部 id 不变）`;
  $("rg-name").value = g.name || "";
  // 成员可能不在网关目录里（组是历史配置、模型已下线）——这些必须以额外 option 出现，
  // 否则「载入表单 → 直接保存」会静默把这些成员删掉。
  const sel = $("rg-members");
  const known = new Set([...sel.options].map((o) => o.value));
  for (const m of g.members || []) {
    if (known.has(m.id)) continue;
    const o = document.createElement("option");
    o.value = m.id; o.textContent = m.id + "（不在当前目录）";
    sel.appendChild(o);
    known.add(m.id);
  }
  const ids = new Set((g.members || []).map((m) => m.id));
  for (const o of sel.options) o.selected = ids.has(o.value);
  if (g.routing && [...$("rg-routing").options].some((o) => o.value === g.routing)) $("rg-routing").value = g.routing;
  if (g.stays && [...$("rg-stays").options].some((o) => o.value === g.stays)) $("rg-stays").value = g.stays;
  $("rg-auto-field").hidden = true;
  $("rg-create").textContent = "保存修改";
  $("rg-cancel").hidden = false;
  $("rg-name").focus();
}

function rgCancelEdit() {
  rgState.editing = "";
  $("rg-form-title").textContent = "新建组";
  $("rg-name").value = "";
  $("rg-auto").value = "";
  $("rg-auto-field").hidden = false;
  for (const o of $("rg-members").options) o.selected = false;
  $("rg-create").textContent = "新建组";
  $("rg-cancel").hidden = true;
}

async function rgSubmit() {
  const name = $("rg-name").value.trim();
  const members = [...$("rg-members").selectedOptions].map((o) => o.value);
  if (!name) { rgFlash(false, "请填组名"); return; }
  if (!members.length) { rgFlash(false, "至少选一个成员模型"); return; }
  const body = { name, members, routing: $("rg-routing").value, stays: $("rg-stays").value };
  const btn = $("rg-create");
  const prev = btn.textContent;
  const editing = rgState.editing;
  btn.disabled = true; btn.textContent = editing ? "保存中…" : "创建中…";
  try {
    if (editing) {
      const v = await api("PUT", "/api/routegroups/" + encodeURIComponent(editing), body);
      rgFlash(true, `已保存 ${(v.group && v.group.public_id) || editing}`);
    } else {
      const autoModel = $("rg-auto").value.trim();
      if (autoModel) body.auto_model = autoModel;
      const v = await api("POST", "/api/routegroups", body);
      rgFlash(true, `已建组 ${(v.group && v.group.public_id) || name}`);
    }
    rgCancelEdit();
    await rgLoad();
    autoSyncModels(lastSyncedModel);
  } catch (e) { rgFlash(false, (editing ? "保存失败：" : "建组失败：") + e.message); }
  btn.disabled = false; btn.textContent = rgState.editing ? "保存修改" : prev;
}

async function rgRemove(internal) {
  const g = rgState.groups.find((x) => x.internal_id === internal);
  if (!g) return;
  const warn = g.found
    ? `删除内置组 ${g.public_id}？\n\n它会变成「已移除」但可以恢复（restore id: ${g.restore_id || internal}）。`
    : `删除自建组 ${g.public_id}？\n\n自建组是真删除，删掉无法恢复（只能用同样的成员重建）。`;
  if (!window.confirm(warn)) return;
  try {
    const v = await api("DELETE", "/api/routegroups/" + encodeURIComponent(internal));
    rgFlash(true, v.hidden ? `已移除 ${g.public_id}（可恢复）` : `已删除 ${g.public_id}`);
    await rgLoad();
    autoSyncModels(lastSyncedModel);
  } catch (e) { rgFlash(false, "删除失败：" + e.message); }
}

async function rgRestore(restoreId) {
  try {
    await api("POST", "/api/routegroups/" + encodeURIComponent(restoreId) + "/restore");
    rgFlash(true, `已恢复 ${restoreId}`);
    await rgLoad();
    autoSyncModels(lastSyncedModel);
  } catch (e) { rgFlash(false, "恢复失败：" + e.message); }
}

document.addEventListener("click", (e) => {
  const btn = e.target.closest && e.target.closest("[data-rg-act]");
  if (!btn) return;
  const act = btn.dataset.rgAct, id = btn.dataset.rgId;
  if (act === "edit") rgEdit(id);
  else if (act === "rm") rgRemove(id);
  else if (act === "restore") rgRestore(id);
});

(function bindRouteGroupPanel() {
  if (!$("rg-head")) return;
  // 组列表要跑 `magpie groups` + 逐组 `group show` 子进程（实测 1~2 秒），
  // 所以只在打开设置或切到 LLM 页签时读，不在脚本加载时就打。
  $("gear").addEventListener("click", () => { if (!rgState.loaded) rgLoad(); });
  document.querySelectorAll('#drawer .tabs button[data-tab="llm"]')
    .forEach((b) => b.addEventListener("click", () => rgLoad()));
  $("rg-create").onclick = rgSubmit;
  $("rg-cancel").onclick = rgCancelEdit;
  $("rg-reload").onclick = () => rgLoad();
  // 模型下拉被自动同步重建（option 全换）时重新判定 auto 组是否存在：
  // 自动同步是改 DOM 而不是派发 change 事件，所以监听 DOM 变化才收得到。
  new MutationObserver(() => rgRenderMissing()).observe($("llm-model"), { childList: true });
  $("llm-model").addEventListener("change", rgRenderMissing);
  $("llm-model-custom").addEventListener("input", rgRenderMissing);
})();

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
// ── 设置面板「📂 源文件」：host 代为在文件管理器中打开配置源文件位置（浏览器禁 file:// 跳转）──
async function reveal(target, name, flashId) {
  try {
    await api("POST", "/api/reveal?target=" + target + (name ? "&name=" + encodeURIComponent(name) : ""));
    flash(flashId, true, "已在文件管理器中打开");
  } catch (e) { flash(flashId, false, e.message); }
}
$("llm-reveal").onclick = () => reveal("config", null, "llm-flash");
// ── F2：记忆页（画像可见化）──────────────────────────────────────────────────
// 设计取舍（对齐 I-20261108-02「设置把『我做的决定』和『系统现在的状态』混成一件」）：
// 1. **能看**：此前画像只在 prompt 里生效，用户完全看不见——写错了也不知道，
//    而它每轮都影响回答。可见化本身就是修 bug。
// 2. **只能删**：新增/编辑走对话（`profile.py`），因为画像是**观察的结论**，
//    用户手写一条「我喜欢简洁」不是观察而是一条命令；命令说在对话里，由模型落成画像。
//    但删除必须是用户的权力：写错的结论会让模型每轮带着它犯同一个错。
// 3. **分层展示**：全局 → 各域 → 观察。域按「作用范围」而非角色分（见 04 文档），
//    UI 直接把这条判据写进每组标题，用户看到结构就学会判据。
const MEM_KIND_LABEL = { user: "关于你", model: "模型脾气", workflow: "工作方式", env: "环境" };
let memData = null;

async function loadMemory() {
  const box = $("mem-body"), stat = $("mem-stat");
  if (!box) return;
  box.innerHTML = '<div class="empty">读取中…</div>';
  try {
    const v = await api("GET", "/api/profile");
    memData = v.profile ?? {};
    renderMemory();
  } catch (e) {
    stat.textContent = "读取失败";
    box.innerHTML = `<div class="empty">画像读取失败: ${esc(e.message)}</div>`;
  }
}

function memCount(p) {
  const secs = (p.sections ?? []).length, notes = (p.notes ?? []).length;
  const doms = Object.keys(p.domains ?? {}).length;
  return `全局 ${secs} 章节 · ${notes} 条观察 · ${doms} 个域`;
}

function renderMemory() {
  const box = $("mem-body"), stat = $("mem-stat");
  if (!box) return;
  const p = memData ?? {};
  box.innerHTML = "";
  const secs = p.sections ?? [], notes = p.notes ?? [], doms = p.domains ?? {};

  if (p.corrupt) {
    box.appendChild(el(`<div class="mem-warn">profile.json 解析失败——已按空画像展示，<b>原文件未改动</b>。请点「📂 源文件」人工核对。</div>`));
  }
  const empty = !secs.length && !notes.length && !Object.keys(doms).length;
  if (!p.exists || (empty && !p.corrupt)) {
    stat.textContent = p.exists ? "画像是空的" : "还没有画像";
    box.appendChild(el(`<div class="empty">还没有记下任何结论。<br>聊天时我若发现稳定的规律（你怎么偏好、这个域怎么做事），会记到这里——不用你手动填。</div>`));
    return;
  }
  stat.textContent = memCount(p) + (p.current_domain ? ` · 当前域 ${p.current_domain}` : " · 未选域（只用全局）");

  // ① 全局：换到哪都对的事实
  box.appendChild(memGroupHead("全局（换到哪都对）", `每轮注入 · 上限内合并`, false));
  const gbody = el(`<div class="mem-g"></div>`);
  if (!secs.length && !notes.length) gbody.appendChild(el(`<div class="empty">全局层为空——所有结论都在域里，或尚未记录</div>`));
  for (const s of secs) gbody.appendChild(memSection(s, ""));
  box.appendChild(gbody);

  // ② 域：同一件事在不同场合的不同版本
  const names = Object.keys(doms);
  if (names.length) {
    box.appendChild(memGroupHead("域（换到别处就错）", `${names.length} 个 · 上限 ${p.max_domains ?? 8}`, false));
    for (const name of names) {
      const d = doms[name] ?? {};
      const cur = name === p.current_domain;
      const head = el(`<div class="mem-domain-head${cur ? " cur" : ""}"></div>`);
      head.innerHTML = `<span class="mem-dname">${esc(name)}</span>` +
        (cur ? `<span class="mem-cur">当前</span>` : "") +
        `<span class="mem-dcnt">${(d.sections ?? []).length} 章节 · ${(d.notes ?? []).length} 观察</span>` +
        `<button class="mem-del" title="删除整个域（只删这个域，不动全局）">🗑</button>`;
      head.querySelector(".mem-del").onclick = () => memDelete("domain-rm", name, "", `域「${name}」`);
      box.appendChild(head);
      const body = el(`<div class="mem-g domain"></div>`);
      for (const s of d.sections ?? []) body.appendChild(memSection(s, name));
      for (const n of d.notes ?? []) body.appendChild(memNote(n, name));
      if (!(d.sections ?? []).length && !(d.notes ?? []).length) {
        body.appendChild(el(`<div class="empty">（空域）</div>`));
      }
      box.appendChild(body);
    }
  }

  // ③ 观察：原始记录（进 prompt 只取最近几条，这里全列——「我记过什么」要能查全）
  if (notes.length) {
    box.appendChild(memGroupHead("观察（原始记录，不是结论）", `${notes.length} 条 · 进上下文只取最近 10 条`, false));
    const nb = el(`<div class="mem-g"></div>`);
    for (const n of notes) nb.appendChild(memNote(n, ""));
    box.appendChild(nb);
  }

  const foot = el(`<p class="hint" style="margin-top:10px"></p>`);
  foot.textContent = "域按「作用范围」分，不按角色分：换到别处会错 → 域；换到哪都对 → 全局。" +
    "系统不会自己猜域（猜错比不猜更坏），切域请让我在对话里做。";
  box.appendChild(foot);
}

function memGroupHead(title, meta) {
  const h = el(`<div class="mem-head"></div>`);
  h.innerHTML = `<span class="mem-t">${esc(title)}</span><span class="mem-m">${esc(meta)}</span>`;
  return h;
}

function memSection(s, domain) {
  const row = el(`<div class="mem-sec"></div>`);
  const kind = MEM_KIND_LABEL[s.kind] ?? s.kind ?? "";
  row.innerHTML =
    `<div class="mem-sh"><span class="mem-kind k-${esc(s.kind ?? "")}">${esc(kind)}</span>` +
    `<span class="mem-sid">${esc(s.id ?? "")}</span>` +
    `<span class="mem-stitle">${esc(s.title ?? "")}</span>` +
    `<button class="mem-del" title="删除这个章节（删了就真不记得了）">🗑</button></div>` +
    `<div class="mem-sbody">${esc(s.body ?? "")}</div>`;
  row.querySelector(".mem-del").onclick = () => memDelete("section-rm", s.id, domain, `章节「${s.title || s.id}」`);
  return row;
}

function memNote(n, domain) {
  const row = el(`<div class="mem-note"></div>`);
  const tags = (n.tags ?? []).map((t) => `<span class="itag">${esc(t)}</span>`).join("");
  row.innerHTML =
    `<span class="mem-kind k-${esc(n.kind ?? "")}">${esc(MEM_KIND_LABEL[n.kind] ?? n.kind ?? "")}</span>` +
    `<span class="mem-ntext">${esc(n.text ?? "")}</span>` +
    (tags ? `<span class="mem-ntags">${tags}</span>` : "") +
    `<span class="mem-nid">${esc(n.id ?? "")}</span>` +
    `<button class="mem-del" title="删掉这条观察">🗑</button>`;
  row.querySelector(".mem-del").onclick = () => memDelete("note-rm", n.id, domain, "这条观察");
  return row;
}

// 删除：两步确认放在**按钮原位**（不用 window.confirm——它会遮住列表，
// 用户看不见自己在删哪一条；与灵感删除同一裁决线）。
async function memDelete(op, id, domain, label) {
  const msg = `删掉${label}？这会真的从 profile.json 里移除。`;
  if (!window.confirm(msg)) return;
  try {
    await api("POST", "/api/profile", { op, id, domain });
    flash("skill-flash", true, `已删除：${label}`);
    await loadMemory();
  } catch (e) {
    flash("skill-flash", false, "删除失败: " + e.message);
  }
}

$("mem-refresh").onclick = () => loadMemory();
$("mem-reveal").onclick = () => reveal("config", null, "skill-flash");

// ── F3：能力总览（设置抽屉的目录页）──────────────────────────────────────────
// 三列 = 三类决定来源（见 index.html 注释）。每列的值都从**已有数据**推出来，
// 不新增任何后端接口：
//   · 你决定的 → loadConfig 已拉到的 llm / agent / tools
//   · 它带的牌 → tools 的 pool 分类 + skillsCache
//   · 它记得的事 → loadMemory 已拉到的 profile
// 点行跳页签，不展开不弹窗——目录的价值是「一眼看清 + 一步到位」。
function ovSet(id, text) {
  const node = $(id);
  if (node) node.textContent = text;
}

function renderOverview() {
  if (!$("cap-overview")) return;
  const c = window.__cfgCache;
  if (c) {
    const m = c.llm ?? {};
    ovSet("ov-llm", m.model ? `${m.provider ?? "?"} · ${m.model}` : (m.provider ?? "未配置"));
    const tools = c.tools ?? [];
    const on = tools.filter((t) => t.enabled).length;
    ovSet("ov-tools", on ? `${on} / ${tools.length} 启用` : "未启用任何工具");
    const a = c.agent ?? {};
    const bits = [];
    if (a.max_rounds) bits.push(`≤${a.max_rounds} 轮`);
    if (a.budget_secs > 0) bits.push(`${a.budget_secs}s`);
    if (a.token_budget > 0) bits.push(`${a.token_budget} tok`);
    ovSet("ov-agent", bits.length ? bits.join(" · ") : "全部用缺省");
    const st = tools.filter((t) => t.pool === "skill").length;
    const mc = tools.filter((t) => t.pool === "mcp").length;
    const mcOn = tools.filter((t) => t.pool === "mcp" && t.enabled).length;
    ovSet("ov-pool", `技能 ${st} · MCP ${mcOn}/${mc}`);
  }
  if (skillsCache.length || c) {
    const preset = skillsCache.filter((s) => s.origin === "preset").length;
    ovSet("ov-skills", skillsCache.length
      ? `${skillsCache.length} 个（内置 ${preset} · 用户 ${skillsCache.length - preset}）`
      : "暂无技能");
  }
  const p = memData;
  if (p) {
    const ns = (p.sections ?? []).length, nn = (p.notes ?? []).length;
    const nd = Object.keys(p.domains ?? {}).length;
    ovSet("ov-mem", p.exists ? `${ns} 章节 · ${nn} 观察 · ${nd} 域` : "还没有画像");
    ovSet("ov-dom", p.current_domain ? p.current_domain : "未选域（只用全局）");
  }
}

// 点行跳页签：复用既有 tabs 的点击逻辑（不另写一套切换，避免两处状态不同步）
function ovGoto(tab) {
  const btn = [...document.querySelectorAll("#drawer .tabs button")].find((b) => b.dataset.tab === tab);
  if (btn) btn.click();
}

document.addEventListener("click", (e) => {
  const row = e.target.closest(".ov-row");
  if (row && row.dataset.goto) ovGoto(row.dataset.goto);
});

// ── 内容分发面板（I-20261108-07）────────────────────────────────────────────
// 设计取舍：面板只做两件事——「显示每个平台能不能用、缺什么」和「开关 + 填凭证」。
// 不在面板里造「选稿件/点发布」的流程：稿件是文件，发布是 `scripts/publish.py send`，
// agent 一条命令就能干，把发布流程搬进 UI 只会多一套要维护的状态机。
// 面板的价值在第 1 件——`sau` 没装、某个平台没登录、AppSecret 没填，这些只能在这里看见。
let pubState = { platforms: [] };

async function loadPublish() {
  const list = $("pub-list");
  if (!list) return;
  try {
    const r = await api("GET", "/api/publish/config");
    pubState = r;
    $("pub-account").value = (r.defaults && r.defaults.account) || "creator";
    renderPublish(r.platforms || []);
  } catch (e) {
    list.innerHTML = `<p class="flash err">读取分发配置失败：${esc(e.message)}</p>`;
  }
}

function renderPublish(rows) {
  const box = $("pub-list");
  if (!rows.length) { box.innerHTML = `<p class="hint">没有可选平台。</p>`; return; }
  box.innerHTML = rows.map((p) => {
    const pcfg = (pubState.platforms || []).find((x) => x.platform === p.platform) || {};
    const ready = p.ready
      ? `<span class="flash ok">就绪</span>`
      : `<span class="flash err">不可用</span>`;
    // 不可用原因要完整露出：用户照着这句话就能去修（装 sau / 登录 / 填 secret）
    const why = p.ready ? "" : `<div class="hint" style="white-space:pre-wrap;color:#c66">${esc(p.reason)}</div>`;
    const kinds = (p.kinds || []).join(" / ");
    const wx = p.platform === "wechat" ? `
      <div class="field"><label>AppID</label><input data-pub="wechat.appid" value="${escAttr(p.appid || "")}" placeholder="wx..."></div>
      <div class="field"><label>AppSecret（保存后只显示掩码）</label><input data-pub="wechat.app_secret" type="password" value="" placeholder="${p.secret_set ? "已设置，留空不修改" : "未设置"}"></div>
      <div class="field"><label>作者名（≤16 字，可空）</label><input data-pub="wechat.author" value="${escAttr(p.author || "")}"></div>
      <label style="display:flex;gap:6px;align-items:center;margin:6px 0">
        <input type="checkbox" data-pub="wechat.auto_publish" ${p.auto_publish ? "checked" : ""}>
        <span>直接发布（不勾=只进草稿箱，更安全）</span>
      </label>` : "";
    return `<div class="ov" style="margin-bottom:10px">
      <label style="display:flex;gap:8px;align-items:center;font-weight:600">
        <input type="checkbox" data-pub="${p.platform}.enabled" ${p.enabled ? "checked" : ""}>
        <span>${esc(p.label)}</span>
        <span class="hint" style="font-weight:400">（可发：${esc(kinds)}）</span>
        ${ready}
      </label>
      ${why}
      ${wx}
    </div>`;
  }).join("");
}

function esc(s) { return String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c])); }
function escAttr(s) { return esc(s); }

document.addEventListener("click", async (e) => {
  if (!e.target.closest || !e.target.closest("#pub-save")) return;
  const platforms = {};
  document.querySelectorAll("[data-pub]").forEach((el) => {
    const [plat, key] = el.dataset.pub.split(".");
    platforms[plat] = platforms[plat] || {};
    if (el.type === "checkbox") platforms[plat][key] = el.checked;
    else if (el.type === "password") { if (el.value) platforms[plat][key] = el.value; }  // 空 = 不修改
    else platforms[plat][key] = el.value;
  });
  try {
    await api("POST", "/api/publish/config", { platforms, defaults: { account: $("pub-account").value || "creator" } });
    flash("pub-flash", true, "已保存，下一轮生效");
    await loadPublish();
  } catch (err) {
    flash("pub-flash", false, "保存失败：" + err.message);
  }
});

// ── 外部通道通用扩展点：整段 JSON → PUT external_channels（后端只接受 string/bool/number，敏感字段留空/掩码对象不改）──
$("channels-save").onclick = async () => {
  const raw = $("channels-json").value.trim();
  if (!raw) { flash("channels-flash", false, "内容为空，未保存"); return; }
  let ext;
  try { ext = JSON.parse(raw); } catch (e) { flash("channels-flash", false, "JSON 解析失败: " + e.message); return; }
  if (!ext || typeof ext !== "object" || Array.isArray(ext)) { flash("channels-flash", false, "必须是 JSON 对象"); return; }
  // 敏感字段回显为 {key_set,key_tail} 掩码对象：这类值表示"未改动"，从提交中剔除（留空=不修改）
  const isMask = (v) => v && typeof v === "object" && !Array.isArray(v) && Object.prototype.hasOwnProperty.call(v, "key_set");
  for (const sec of Object.values(ext)) {
    if (!sec || typeof sec !== "object" || Array.isArray(sec)) continue;
    for (const k of Object.keys(sec)) if (isMask(sec[k])) delete sec[k];
  }
  if (!Object.keys(ext).length) { flash("channels-flash", true, "无改动，已跳过保存"); loadConfig(); return; }
  try {
    await api("PUT", "/api/config", { external_channels: ext });
    flash("channels-flash", true, "已保存（消费方下次读取生效）");
    loadConfig();
  } catch (e) { flash("channels-flash", false, e.message); }
};

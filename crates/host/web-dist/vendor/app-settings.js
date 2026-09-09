"use strict";
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
    // 组头状态徽章（pcnt 位）：配置一眼可见
    $("llm-cnt").textContent = c.llm.model || c.llm.provider || "未配置";
    $("media-img-cnt").textContent = mi.base_url ? "已配置" : "未配置";
    $("media-vid-cnt").textContent = (mv.base_url && (mv.submit_url || mv.query_url)) ? "已配置" : "未配置";
    cfgTools = c.tools ?? [];
    cfgMcp = c.mcp ?? { servers: [], tools: [] };
    if (!$("tab-tools").hidden) renderTools(); // 工具 tab 可见时回填（覆盖 fetch 完成晚于 tab 切换的时序）
    // （MCP key 配置已移入「MCP 外接」各服务组内，随 renderTools 渲染）
    syncLlmFields();
    showInstBadge(c.instance_name ?? ""); // W17：子实例头部徽章（主实例为空不显示）
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
// LLM tab 三区折叠（与工具池 poolhead 同款交互；静态 HTML，加载时绑定一次）
document.querySelectorAll("#tab-llm .poolhead.collapsible").forEach((head) => {
  const body = head.nextElementSibling;
  head.onclick = () => {
    body.hidden = !body.hidden;
    head.querySelector(".chev").textContent = body.hidden ? "▸" : "▾";
    head.classList.toggle("closed", body.hidden);
  };
});
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

"use strict";
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
  const card = el(`<div class="artcard"><span class="ico">📄</span><span class="nm">${esc(name)}</span><span class="src" title="${esc(ev.path)}">${esc(ev.path)}${ev.tool ? " · " + esc(ev.tool) : ""}</span><span class="open">${PREVIEW_TEXT_EXTS.includes(ext) || PREVIEW_NATIVE_EXTS.includes(ext) || PREVIEW_VIDEO_EXTS.includes(ext) ? "查看" : "下载"}</span></div>`);
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
    // 关闭即清空 body：视频预览时停止播放（音频不残留）；Monaco 已先 dispose，无副作用
    const close = () => { disposePreviewEditor(); overlay.querySelector(".fp-body").innerHTML = ""; overlay.style.display = "none"; };
    overlay.onclick = (e) => { if (e.target === overlay) close(); };
    overlay.querySelector(".fp-close").onclick = close;
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

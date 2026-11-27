// memory guest 插件（TypeScript，Process 域 gRPC）。
//
// 线契约：
//   {"op":"append","session_id":str,"messages":[Msg...]} → {"ok":true,"count":int}
//   {"op":"get","session_id":str,"limit"?:int}           → {"ok":true,"messages":[Msg...]}
//   {"op":"clear","session_id":str}                      → {"ok":true}
//   {"op":"summarize","session_id":str,"summary":str,"keep_last"?:int}
//       → {"ok":true,"count":int}   —— 上下文压缩（Phase 2-2）：历史替换为
//         [压缩标记消息] + 最近 keep_last 条（默认 10）；孤儿 tool 消息防撕裂。
//   {"op":"trace.append","session_id":str,"events":[Event...]} → {"ok":true,"count":int}
//   {"op":"trace.read","session_id":str,"after"?:int}          → {"ok":true,"events":[Event],"next":int}
//   {"op":"rollback","session_id":str,"upto_user_index":int}   → {"ok":true,"removed_messages","removed_events"}
//       R2 回滚（物理截断语义）：按「第 upto_user_index 条 user 消息」（0 基）把该条及
//       之后的一切截掉——会话消息（LLM 上下文）与 trace 事件日志（UI 重放）原子同滚，
//       先算两侧切点、任一越界即整体失败不落盘。user 计数口径以 trace user 事件为准
//       （UI 真相源，前端气泡序号即此）；memory 经压缩只留「标记 + 最近 K 条」，两侧
//       user 数天然不对齐 → 尾部对齐（压缩只裁头部）：回滚点在保留区 → 保标记截到
//       该轮前；落在摘要区 → 标记与消息全清（摘要与回滚区间重叠，保留即上下文残留，
//       agent 会误以为已回滚的材料还在）。无 trace 文件的纯 memory 会话按 memory 侧
//       真实 user 计数定位，压缩标记随截断一并丢弃。
//   {"op":"memory.recall","session_id":str,"query"?:str,"limit"?:int}
//       → {"ok":true,"items":[{kind,ts,session_id,cursor,preview}]} —— 摘要级召回（≤5 条）
//   {"op":"session.index"} → {"ok":true,"sessions":[{id,title,updated_at,event_count,last_cursor}]}
//   {"op":"trace.meta","session_id":str} → {"ok":true,"exists":bool,"size":int,"mtime_ms":int,"total_events":int,"end_cursor":int}  —— 完整 trace 的有效事件总数与真实末尾绝对游标（M 轮新增）
//   {"op":"session.skills","session_id":str} → {"ok":true,"skills":[str],"total_events":int}  —— 会话级已加载技能（扫描完整 trace，去重保序；M 轮新增）
//       以上 trace.meta / session.skills 为 M 轮（长 trace 绝对游标 + 会话级技能作用域）纯追加 op；旧 op 语义逐字不变。
//       以上为 Q 轮 Phase 2 纯追加 op；旧 op 语义逐字不变（工作集视图仍为 sessions/<safe>.json），
//       原文/压缩/回滚/UI 事件另追加到 events/<safe>.jsonl（append-only sidecar）。
// 服务于审计/恢复/UI 重放（与 memory「模型上下文」是不同关注点，dsh：Model-visible means logged）。
// Msg = {role, content?, tool_calls?, tool_call_id?} —— 与 agent-loop 的 contract.rs 镜像。
//
// 存储：内存 Map + 每会话一个 JSON 文件（MEMORY_DATA_DIR 或 ./data/sessions/）。
// Semantics::Serial 语义下同步读写安全。
import { serve, type Plugin, type PluginManifest } from "./guest_sdk.ts";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

interface ToolCall {
  id: string;
  name: string;
  arguments: unknown;
}
interface Msg {
  role: string;
  content?: string | null;
  tool_calls?: ToolCall[];
  tool_call_id?: string;
}

function dataDir(): string {
  const configured = process.env.MEMORY_DATA_DIR;
  if (configured) return configured;
  const here = path.dirname(fileURLToPath(import.meta.url));
  return path.join(here, "data");
}

function sessionsDir(): string {
  const dir = path.join(dataDir(), "sessions");
  fs.mkdirSync(dir, { recursive: true });
  return dir;
}

function sessionFile(id: string): string {
  // 会话 id 仅作为文件名；过滤路径分隔符防穿越
  const safe = id.replace(/[^A-Za-z0-9._-]/g, "_");
  return path.join(sessionsDir(), `${safe}.json`);
}

const tracesDir = (): string => {
  const dir = path.join(dataDir(), "traces");
  fs.mkdirSync(dir, { recursive: true });
  return dir;
};

const traceFile = (id: string): string =>
  path.join(tracesDir(), `${id.replace(/[^A-Za-z0-9._-]/g, "_")}.jsonl`);

const sessions = new Map<string, Msg[]>();

function loadIfAbsent(sessionId: string): Msg[] {
  let msgs = sessions.get(sessionId);
  if (msgs) return msgs;
  const file = sessionFile(sessionId);
  let text = "";
  try {
    text = fs.readFileSync(file, "utf8");
  } catch {
    /* 尚无会话文件：正常路径 */
  }
  if (text) {
    try {
      msgs = JSON.parse(text) as Msg[];
    } catch (e) {
      // P4：半文件/损坏——过去静默当空会话，数据丢失无告警。响亮提示但保持可用
      // （配 persist 的 tmp+rename 原子写后，新数据不应再出现此路径）。
      console.error(`[memory] 会话文件解析失败（可能为历史半文件），按空会话继续: ${file} (${e})`);
      msgs = [];
    }
  } else {
    msgs = [];
  }
  sessions.set(sessionId, msgs);
  return msgs;
}

function persist(sessionId: string, msgs: Msg[]): void {
  const file = sessionFile(sessionId);
  if (msgs.length === 0) {
    try {
      fs.rmSync(file, { force: true });
    } catch {
      /* 首次 clear 前 file 可能不存在 */
    }
    return;
  }
  // P4：原子写（tmp + rename）——writeFileSync 直写在崩溃/断电时可能留下半文件，
  // 读侧解析失败会静默当空会话（整段会话记忆蒸发且无告警）。同目录 tmp 保证同盘 rename。
  const tmp = `${file}.tmp-${process.pid}`;
  fs.writeFileSync(tmp, JSON.stringify(msgs), "utf8");
  fs.renameSync(tmp, file);
}

// P4：trace 行索引（假分页治理）。trace.read 过去每次轮询（网关 300ms 一次）全文件重读
// + 全行 JSON.parse（O(全文件)）；大 trace（5MB+）下每 300ms 一次全量解析纯烧 CPU。
// sidecar 缓存 = {文件size, mtimeMs, 各事件行字节偏移}：两者与当前文件一致 → 按偏移
// 随机直读所需页（O(页)）；文件有增长 → 全量重建一次并写回（生成期仍 O(N)，稳态 O(1)）。
const traceIdx = new Map<string, { size: number; mtimeMs: number; offsets: number[] }>();

function traceOffsets(file: string): number[] {
  const st = fs.statSync(file);
  const hit = traceIdx.get(file);
  if (hit && hit.size === st.size && hit.mtimeMs === st.mtimeMs) return hit.offsets;
  const offsets: number[] = [];
  let offset = 0;
  for (const line of fs.readFileSync(file, "utf8").split("\n")) {
    const t = line.trim();
    if (t) {
      try {
        JSON.parse(t);
        offsets.push(offset);
      } catch {
        /* 损坏行（半行写入容忍）：不进索引 */
      }
    }
    offset += Buffer.byteLength(line + "\n", "utf8");
  }
  traceIdx.set(file, { size: st.size, mtimeMs: st.mtimeMs, offsets });
  return offsets;
}

// X3（首屏性能，实测 67x）：每条事件都读满 1MB 的代价。
// 原实现 `Math.min(1 << 20, size - off)` 是「从 off 读到文件尾，但最多 1MB」——
// 实际每次分配并读入 1MB，只为取出其中的第一行。main.jsonl 有 14568 条事件、
// 平均行长 1027B：单批 800 条就读 800MB，全量重放读入 14.2GB（文件才 14MB），
// 实测 32s，页面打开看到的就是长时间空白。
// 索引 (traceOffsets) 本来就有下一行的字节偏移，传进来即可按行长精确读：
// 单批 1757ms → 26ms。`end` 缺省（末行 / 未知）时才退回原来的 1MB 上限。
function readEventLine(file: string, off: number, end?: number): unknown {
  const fd = fs.openSync(file, "r");
  try {
    const cap = Math.min(1 << 20, fs.fstatSync(fd).size - off); // 单事件上限 1MB（与分页字节预算同量级）
    if (cap <= 0) return null;
    const len = end !== undefined && end > off ? Math.min(end - off, cap) : cap;
    const buf = Buffer.alloc(len);
    fs.readSync(fd, buf, 0, len, off);
    const t = buf.toString("utf8").split("\n")[0].trim();
    try {
      return JSON.parse(t);
    } catch {
      return null;
    }
  } finally {
    fs.closeSync(fd);
  }
}

function err(message: string): { ok: false; error: { message: string } } {
  return { ok: false, error: { message } };
}

// ── Q 轮 Phase 2：append-only sidecar + session.index + recall + TTL ──────────
// 设计取舍（「换底座、不换视图」）：sessions/<safe>.json 仍是 LLM 工作集的唯一
// 视图，get/summarize/rollback 语义与旧版逐字一致（e2e 断言不破）；所有原始
// 消息、压缩摘要、trace 事件另行追加到 events/<safe>.jsonl，原文永不因压缩删除。
// SessionEvent = {ts, kind, ...}，白名单字段投影供 memory.recall 摘要级召回。
const eventsDir = (): string => {
  const dir = path.join(dataDir(), "events");
  fs.mkdirSync(dir, { recursive: true });
  return dir;
};
const eventFile = (id: string): string =>
  path.join(eventsDir(), `${id.replace(/[^A-Za-z0-9._-]/g, "_")}.jsonl`);
const indexFile = (): string => path.join(dataDir(), "session.index.json");

function appendSessionEvents(sessionId: string, rows: unknown[]): void {
  if (!rows.length) return;
  const ts = Date.now();
  fs.appendFileSync(
    eventFile(sessionId),
    rows.map((e) => JSON.stringify({ ts, ...(e as Record<string, unknown>) })).join("\n") + "\n",
    "utf8",
  );
  touchIndex(sessionId, rows.length);
}

type SessionIndexEntry = {
  id: string; title: string; updated_at: number; event_count: number; last_cursor: number;
};
type SessionIndex = { sessions: SessionIndexEntry[] };
function loadIndex(): SessionIndex {
  try {
    const v = JSON.parse(fs.readFileSync(indexFile(), "utf8")) as SessionIndex;
    if (v && Array.isArray(v.sessions)) return v;
  } catch { /* 尚无索引：正常路径 */ }
  return { sessions: [] };
}
function saveIndex(idx: SessionIndex): void {
  // tmp+rename 原子写（与 persist 同款纪律）；索引是派生数据，损坏可重建。
  const file = indexFile();
  const tmp = `${file}.tmp-${process.pid}`;
  fs.writeFileSync(tmp, JSON.stringify(idx), "utf8");
  fs.renameSync(tmp, file);
}
function touchIndex(sessionId: string, delta = 1): void {
  try {
    const idx = loadIndex();
    let it = idx.sessions.find((s) => s.id === sessionId);
    if (!it) { it = { id: sessionId, title: "", updated_at: 0, event_count: 0, last_cursor: 0 }; idx.sessions.unshift(it); }
    it.updated_at = Date.now();
    it.event_count += delta;
    it.last_cursor = it.event_count;
    // TTL/空间卫生：只保留最近 200 个活跃会话的元数据（文件系统仍是真相源，
    // 老会话再次 append 时自动重建条目——filesystem-first 修 stale 行）。
    idx.sessions.sort((a, b) => b.updated_at - a.updated_at);
    idx.sessions = idx.sessions.slice(0, 200);
    saveIndex(idx);
  } catch (e) {
    console.error(`[memory] session.index 更新失败（不影响主链）: ${e}`);
  }
}
function pruneSessionCache(now: number): void {
  // M6：sessions Map 永不逐出会让长驻进程无界增长。30 分钟未访问的会话从内存
  // 逐出，磁盘视图与 sidecar 不动（下次 get 由 loadIfAbsent 冷加载）。
  const TTL_MS = 30 * 60 * 1000;
  for (const [id, last] of sessionTouched) {
    if (now - last > TTL_MS) { sessions.delete(id); sessionTouched.delete(id); }
  }
}
const sessionTouched = new Map<string, number>();

function recall(sessionId: string, query: string, limit: number): unknown[] {
  // 摘要级召回：不返回全文、不返回附件 base64、不返回密钥型字段。
  const q = String(query || "").toLowerCase().trim();
  const max = Math.min(Math.max(1, Math.floor(limit || 5)), 5);
  const out: unknown[] = [];
  let text = "";
  try { text = fs.readFileSync(eventFile(sessionId), "utf8"); } catch { return out; }
  const lines = text.split("\n").filter(Boolean).slice(-2000); // 只扫最近 2000 行
  for (let i = lines.length - 1; i >= 0 && out.length < max; i--) {
    let e: Record<string, unknown>;
    try { e = JSON.parse(lines[i]) as Record<string, unknown>; } catch { continue; }
    const kind = String(e.kind ?? "");
    const preview = String(e.preview ?? e.summary ?? e.text ?? "").slice(0, 200);
    if (q && !`${kind} ${preview}`.toLowerCase().includes(q)) continue;
    out.push({ kind, ts: e.ts ?? 0, session_id: sessionId, cursor: i + 1, preview });
  }
  return out;
}

export const memoryPlugin: Plugin = {
  manifest(): PluginManifest {
    return { id: "memory", version: "0.1.0", api_version: "0.1" };
  },
  init(_config: unknown): void {
    fs.mkdirSync(sessionsDir(), { recursive: true });
    fs.mkdirSync(tracesDir(), { recursive: true });
    fs.mkdirSync(eventsDir(), { recursive: true });  // Q 轮：append-only sidecar 目录
    if (!fs.existsSync(indexFile())) saveIndex({ sessions: [] }); // init/destroy 成对：派生索引有初态
  },
  onEvent(envelope: { target: unknown; payload: unknown }): unknown {
    const payload = (envelope.payload ?? {}) as Record<string, unknown>;
    const op = payload.op as string | undefined;
    const sessionId = payload.session_id as string | undefined;
    if (!sessionId) return err("session_id is required");
    switch (op) {
      case "append": {
        const incoming = (payload.messages ?? []) as Msg[];
        const msgs = loadIfAbsent(sessionId);
        msgs.push(...incoming);
        persist(sessionId, msgs);
        // Q 轮：原文同步追加进 sidecar（工作集视图仍是 sessions/<safe>.json）。
        appendSessionEvents(sessionId, incoming.map((m) => ({
          kind: "message", role: m.role, preview: (m.content ?? "").slice(0, 200),
        })));
        return { ok: true, count: msgs.length };
      }
      case "get": {
        sessionTouched.set(sessionId, Date.now());
        pruneSessionCache(Date.now());
        const msgs = loadIfAbsent(sessionId);
        const limit = payload.limit as number | undefined;
        return { ok: true, messages: limit ? msgs.slice(-limit) : msgs };
      }
      case "clear": {
        sessions.set(sessionId, []);
        persist(sessionId, []);
        return { ok: true };
      }
      case "summarize": {
        // 上下文压缩（Phase 2-2）：摘要 + 最近 keep_last 条。防撕裂：切片后丢弃
        // 开头的孤儿 tool 消息（其 assistant(tool_calls) 载体已被裁掉）。
        const summary = (payload.summary as string | undefined)?.trim();
        if (!summary) return err("summary is required");
        const raw = payload.keep_last as number | undefined;
        const keepLast = raw === undefined || raw < 0 ? 10 : Math.floor(raw);
        const msgs = loadIfAbsent(sessionId);
        // M3：system 保留必须统一且规范化（与 agent-loop 同规则）。
        // - 不假定 system 在 msgs[0]：持久化历史里的 system 可能位于任意位置；
        // - 全历史最多保留**一个** system，取最后一条（最接近当前动态 system 的那份），
        //   绝不保留多个（多 system 会与动态 system 冲突、污染上下文）；
        // - 放在压缩标记之前：标记是 user 角色，system 必须最前；
        // - 空内容 system 视为无 system（与 agent-loop 的规范化判定一致）。
        let sys: Msg | undefined;
        for (let i = msgs.length - 1; i >= 0; i--) {
          if (msgs[i].role === "system" && typeof msgs[i].content === "string" && (msgs[i].content as string).trim().length > 0) {
            sys = msgs[i];
            break;
          }
        }
        // 受保护的头：system 不算「最近」条目——从池中剔除**所有** system，只保留上面选定的
        // 唯一 sys 前置，确保结果里绝不出第二个 system（多 system 会与动态 system 冲突）。
        const pool = msgs.filter((m) => m.role !== "system");
        let kept = keepLast > 0 ? pool.slice(-keepLast) : [];
        while (kept.length > 0 && kept[0].role === "tool") {
          kept = kept.slice(1);
        }
        const marker: Msg = {
          role: "user",
          content:
            `[Context compaction] 之前的会话历史已压缩为以下摘要：\n${summary}\n` +
            `请基于该摘要与后续消息继续任务，不要声称记得被压缩的原文。`,
        };
        const compacted: Msg[] = sys ? [sys, marker, ...kept] : [marker, ...kept];
        sessions.set(sessionId, compacted);
        persist(sessionId, compacted);
        // Q 轮：压缩不再等于删除原文——先追加一条 typed compaction 事件
        // （摘要 + first_kept + tokens_before），工作集视图仍按旧语义替换，
        // 后续可从 sidecar 重放/审计/分支，M1/M4/M10 的「破坏性覆写」消解。
        appendSessionEvents(sessionId, [{
          kind: "compaction", summary, first_kept: msgs.length - kept.length,
          tokens_before: JSON.stringify(msgs).length, kept: kept.length,
        }]);
        return { ok: true, count: compacted.length };
      }
      case "rollback": {
        // R2 回滚（物理截断语义）：按「第 upto_user_index 条 user 消息」（0 基）截断。
        // 原子性：消息侧与 trace 侧切点先全部算好，任一越界即整体失败、不落任何一盘。
        const raw = payload.upto_user_index as unknown;
        if (typeof raw !== "number" || !Number.isInteger(raw) || raw < 0) {
          return err("upto_user_index 必须是非负整数");
        }
        const upto = raw as number;
        // 真实用户轮次判定：压缩标记是 user 角色的合成消息，不计入轮次
        // （与 agent-loop compaction_marker / 本插件 summarize 的固定前缀对齐）。
        const isRealUser = (m: Msg) =>
          m.role === "user" && !(m.content ?? "").startsWith("[Context compaction]");
        // trace 切点先行（UI 真相源）：气泡由 trace 重放渲染，前端下发的 upto 就是
        // trace user 事件序号。memory 侧经压缩只留「标记 + 最近 K 条」，两侧 user 数
        // 天然不对齐、不能互为校验 → 尾部对齐（压缩只裁头部）：memory 的 N 条真实
        // user 恰对应 trace 末尾 N 条 user 事件，local = upto - (trace侧user总数 - N)。
        const file = traceFile(sessionId);
        let text = "";
        try {
          text = fs.readFileSync(file, "utf8");
        } catch {
          /* 尚无 trace 文件：仅回滚消息 */
        }
        // 全量扫描：user 事件行偏移 + 其事件序号（截断长度与 removed_events 都要用）
        const userMarks: Array<{ off: number; evIdx: number }> = [];
        let totalEvents = 0;
        let offset = 0;
        for (const line of text.split("\n")) {
          const lineBytes = Buffer.byteLength(line + "\n", "utf8");
          const t = line.trim();
          if (t) {
            try {
              if ((JSON.parse(t) as { type?: string }).type === "user") {
                userMarks.push({ off: offset, evIdx: totalEvents });
              }
            } catch {
              /* 半行写入容忍：跳过损坏行 */
            }
            totalEvents++;
          }
          offset += lineBytes;
        }
        if (text && upto >= userMarks.length) {
          return err(`回滚位置超出会话历史（共 ${userMarks.length} 条 user 消息）`);
        }
        const msgs = loadIfAbsent(sessionId);
        const realIdx: number[] = [];
        for (let i = 0; i < msgs.length; i++) {
          if (isRealUser(msgs[i])) realIdx.push(i);
        }
        let kept: Msg[];
        if (text) {
          // 尾部对齐定消息切点。local >= 0：该轮还在（未压缩保留区）→ 截到它之前，
          // 标记摘要描述的是切点之前的已压缩历史，与本次回滚不重叠，保留。
          // local < 0：该轮已被压缩进标记（回滚点落在摘要区内）→ 摘要描述的历史与
          // 回滚区间重叠，标记与全部消息一起清空——保留即上下文残留。
          const local = upto - (userMarks.length - realIdx.length);
          kept = local >= 0 ? msgs.slice(0, realIdx[local]) : [];
        } else {
          // 无 trace 文件的纯 memory 会话：按 memory 侧轮次定位，越界即失败；
          // 该轮是否落在摘要区无法判定，压缩标记随截断一并丢弃（防上下文残留）。
          if (upto >= realIdx.length) {
            return err(`回滚位置超出会话历史（共 ${realIdx.length} 条 user 消息）`);
          }
          kept = msgs.slice(0, realIdx[upto]);
          while (kept.length > 0 && (kept[0].content ?? "").startsWith("[Context compaction]")) {
            kept = kept.slice(1);
          }
        }
        sessions.set(sessionId, kept);
        persist(sessionId, kept);
        if (text) {
          fs.truncateSync(file, userMarks[upto].off);
        }
        // Q 轮：物理回滚在工作集/trace 视图保持原语义；sidecar 追加一条 typed
        // rollback 事件，保留「曾经发生过什么」的审计轨迹（原文不复活到工作集）。
        appendSessionEvents(sessionId, [{
          kind: "rollback", upto_user_index: upto,
          removed_messages: msgs.length - kept.length,
          removed_events: text ? totalEvents - userMarks[upto].evIdx : 0,
        }]);
        return {
          ok: true,
          removed_messages: msgs.length - kept.length,
          removed_events: text ? totalEvents - userMarks[upto].evIdx : 0,
        };
      }
      default: {
        // 事件日志（Phase 3-1）：trace.* 不需要会话消息状态，仅要求 session_id（上方已校验）
        if (op === "trace.append") {
          const events = (payload.events ?? []) as unknown[];
          if (!Array.isArray(events) || events.length === 0) return err("events must be a non-empty array");
          fs.appendFileSync(traceFile(sessionId), events.map((e) => JSON.stringify(e)).join("\n") + "\n", "utf8");
          // Q 轮：UI 真相源事件同步进 sidecar（白名单投影，不复制全文/附件 b64）。
          appendSessionEvents(sessionId, events.map((e) => {
            const ev = (e ?? {}) as Record<string, unknown>;
            return {
              kind: "trace", type: ev.type ?? "",
              preview: typeof ev.text === "string" ? ev.text.slice(0, 200)
                : typeof ev.message === "string" ? ev.message.slice(0, 200)
                : typeof ev.answer === "string" ? ev.answer.slice(0, 200) : "",
            };
          }));
          return { ok: true, count: events.length };
        }
        // Q 轮：摘要级召回（不返回全文 / 附件 / 密钥字段），硬上限 5 条。
        if (op === "memory.recall") {
          const query = payload.query as string | undefined;
          const limit = typeof payload.limit === "number" ? payload.limit : 5;
          return { ok: true, items: recall(sessionId, String(query ?? ""), limit) };
        }
        // Q 轮：会话索引（filesystem-first 的派生元数据，供列表/预取）。
        if (op === "session.index") {
          return { ok: true, ...loadIndex() };
        }
        if (op === "trace.read") {
          // P4：按索引偏移直读（traceOffsets 内部 size+mtime 命中缓存 / 未命中重建一次）。
          // after=N 返回第 N 条之后的事件（UI 增量拉取）——语义与全量扫描版完全一致。
          const file = traceFile(sessionId);
          let offsets: number[] = [];
          let total = 0;
          try {
            offsets = traceOffsets(file);
            total = offsets.length;
          } catch {
            /* 尚无日志 → 空事件 */
          }
          const after = typeof payload.after === "number" && payload.after >= 0 ? Math.floor(payload.after) : 0;
          // 分页：一次响应必须远低于 kernel gRPC 解码上限（4MB）。整份大 trace（如 5MB
          // 会话日志）一次性返回会触发 K700「decoded message length too large」——网关
          // 每次轮询都报错，前端把错误帧当断线反复重连、feed 反复清空（页面持续闪烁）。
          // 按「条数 + 字节」双预算切片，next 为已消费游标，网关多批拉取即可追平。
          // 单条超大事件（极端情况）仍单独放行，避免游标不前进造成死循环。
          const MAX_EVENTS = 800;
          const MAX_BYTES = 1_000_000;
          const events: unknown[] = [];
          let bytes = 0;
          let cursor = after;
          while (cursor < total && events.length < MAX_EVENTS) {
            // X3：传入下一条事件的起始偏移，readEventLine 据此只读本行字节
            // （末条无后继 → 传 undefined，退回文件尾/1MB 上限）。
            const ev = readEventLine(file, offsets[cursor], offsets[cursor + 1]);
            if (ev === null) {
              cursor++;
              continue;
            }
            const size = Buffer.byteLength(JSON.stringify(ev), "utf8") + 1;
            if (events.length > 0 && bytes + size > MAX_BYTES) break;
            events.push(ev);
            bytes += size;
            cursor++;
          }
          return { ok: true, events, next: cursor };
        }
        // M1：trace.meta —— 完整 trace 的**有效事件总数**与**真实末尾绝对游标**。
        //
        // 为什么单列一个 op 而不是改 trace.read.next：trace.read.next 是「本页消费位置」
        // （前端增量拉取已消费到第几条），分页受 800 条/约 1MB 双预算约束（见上方
        // trace.read）；把 next 改成总末尾会破坏增量重放与网关追赶（gateway.rs 依赖
        // next 前进来分批追平）。故真实末尾只能由独立 op 提供：这里直接取
        // traceOffsets(file).length —— 即完整有效事件数（跳过损坏行）。
        //
        // 缺 trace 文件时返回空状态（ok:true + total/end_cursor=0 + exists:false），
        // **不是失败**：agent-loop 借此把「尚无事件」与「读取失败」区分开，失败才回退
        // 保守游标（0 / start_cursor / turn 原 end_cursor）。
        if (op === "trace.meta") {
          const file = traceFile(sessionId);
          let exists = false;
          let size = 0;
          let mtimeMs = 0;
          let total = 0;
          try {
            const st = fs.statSync(file);
            exists = true;
            size = st.size;
            mtimeMs = st.mtimeMs;
          } catch {
            /* 尚无 trace 文件：空状态 */
          }
          if (exists) {
            try {
              total = traceOffsets(file).length;
            } catch {
              total = 0;
            }
          }
          return {
            ok: true,
            exists,
            size,
            mtime_ms: mtimeMs,
            total_events: total,
            end_cursor: total,
          };
        }
        // M2：session.skills —— 会话级已加载技能集合。
        //
        // 走 traceOffsets + readEventLine 扫描**完整** trace（不是只看第一页）：过去
        // agent-loop 用 trace.read(after=0) 只读第一页，长 trace 里第 800 条之后的
        // skill_loaded 会被漏掉。技能状态是「会话级事实」，应由 Memory 从 trace 这个
        // 唯一真相源汇总后提供给编排层，而不是把 trace 内部分页细节泄漏给 agent-loop。
        // 返回去重、保持首次出现顺序的技能名（type==="skill_loaded" 且 skill 为非空字符串）。
        if (op === "session.skills") {
          const file = traceFile(sessionId);
          const skills = new Set<string>();
          let total = 0;
          try {
            const offsets = traceOffsets(file);
            total = offsets.length;
            for (let i = 0; i < offsets.length; i++) {
              const ev = readEventLine(file, offsets[i], offsets[i + 1]);
              if (ev === null) continue;
              const e = ev as { type?: unknown; skill?: unknown };
              if (e.type === "skill_loaded" && typeof e.skill === "string" && e.skill.length > 0) {
                skills.add(e.skill);
              }
            }
          } catch {
            /* 尚无 trace 文件 → 空集合 */
          }
          return { ok: true, skills: Array.from(skills), total_events: total };
        }
        return err(`unknown op: ${String(op)}`);
      }
    }
  },
  destroy(): void {
    // Q 轮：无后台 writer/句柄可关（同步 fs 每次写已落盘）；显式清空内存缓存与
    // 访问时间戳，保持 init/destroy 的时空闭环，sidecar / index 作为磁盘真相源保留。
    sessions.clear();
    sessionTouched.clear();
    traceIdx.clear();
  },
};

serve(memoryPlugin);

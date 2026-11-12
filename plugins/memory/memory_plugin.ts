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
// Event = 任意 JSON 对象（建议 {type, ts, ...}）——事件日志（Phase 3-1）：只追加、不可变，
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

export const memoryPlugin: Plugin = {
  manifest(): PluginManifest {
    return { id: "memory", version: "0.1.0", api_version: "0.1" };
  },
  init(_config: unknown): void {
    fs.mkdirSync(sessionsDir(), { recursive: true });
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
        return { ok: true, count: msgs.length };
      }
      case "get": {
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
        let kept = keepLast > 0 ? msgs.slice(-keepLast) : [];
        while (kept.length > 0 && kept[0].role === "tool") {
          kept = kept.slice(1);
        }
        const compacted: Msg[] = [
          {
            role: "user",
            content:
              `[Context compaction] 之前的会话历史已压缩为以下摘要：\n${summary}\n` +
              `请基于该摘要与后续消息继续任务，不要声称记得被压缩的原文。`,
          },
          ...kept,
        ];
        sessions.set(sessionId, compacted);
        persist(sessionId, compacted);
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
          return { ok: true, count: events.length };
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
        return err(`unknown op: ${String(op)}`);
      }
    }
  },
  destroy(): void {
    /* 无需清理：每次写操作已即时落盘 */
  },
};

serve(memoryPlugin);

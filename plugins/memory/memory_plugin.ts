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
//       先算两侧切点、任一越界即整体失败不落盘。user 计数口径 = 真实用户轮次；
//       压缩标记（[Context compaction] 前缀的 user 消息）与 trace 的 compaction 事件不计。
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
  try {
    msgs = JSON.parse(fs.readFileSync(file, "utf8")) as Msg[];
  } catch {
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
  fs.writeFileSync(file, JSON.stringify(msgs), "utf8");
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
        const msgs = loadIfAbsent(sessionId);
        let msgCut = -1;
        let userCount = 0;
        for (let i = 0; i < msgs.length; i++) {
          if (isRealUser(msgs[i])) {
            if (userCount === upto) {
              msgCut = i;
              break;
            }
            userCount++;
          }
        }
        if (msgCut < 0) {
          return err(`回滚位置超出会话历史（共 ${userCount} 条 user 消息）`);
        }
        // trace 切点：第 upto 条 type==="user" 事件行的字节偏移（JSONL 逐行解析，
        // 与消息侧同一计数口径——chat_run 每轮先 append user 消息、再 trace user 事件）
        const file = traceFile(sessionId);
        let text = "";
        try {
          text = fs.readFileSync(file, "utf8");
        } catch {
          /* 尚无 trace 文件：仅回滚消息 */
        }
        let traceCut = -1;
        let evCount = 0;
        let totalEvents = 0;
        let beforeCut = 0;
        let offset = 0;
        for (const line of text.split("\n")) {
          const lineBytes = Buffer.byteLength(line + "\n", "utf8");
          const t = line.trim();
          if (t) {
            totalEvents++;
            try {
              if ((JSON.parse(t) as { type?: string }).type === "user") {
                if (evCount === upto) {
                  traceCut = offset;
                  break;
                }
                evCount++;
              }
            } catch {
              /* 半行写入容忍：跳过损坏行 */
            }
            beforeCut = totalEvents;
          }
          offset += lineBytes;
        }
        if (text && traceCut < 0) {
          return err(`trace 与消息不同源：trace 中只有 ${evCount} 条 user 事件`);
        }
        const removedEvents = text && traceCut >= 0 ? totalEvents - beforeCut : 0;
        // 落盘：消息 + trace 一起截
        const kept = msgs.slice(0, msgCut);
        sessions.set(sessionId, kept);
        persist(sessionId, kept);
        if (text && traceCut >= 0) {
          fs.truncateSync(file, traceCut);
        }
        return { ok: true, removed_messages: msgs.length - msgCut, removed_events: removedEvents };
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
          // 只追加文件整体重读，按行切事件；after=N 返回第 N 条之后的事件（UI 增量拉取）
          let text = "";
          try {
            text = fs.readFileSync(traceFile(sessionId), "utf8");
          } catch {
            /* 尚无日志 → 空事件 */
          }
          const all: unknown[] = [];
          for (const line of text.split("\n")) {
            const t = line.trim();
            if (!t) continue;
            try {
              all.push(JSON.parse(t));
            } catch {
              /* 半行写入容忍：跳过损坏行 */
            }
          }
          const after = typeof payload.after === "number" && payload.after >= 0 ? Math.floor(payload.after) : 0;
          return { ok: true, events: all.slice(after), next: all.length };
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

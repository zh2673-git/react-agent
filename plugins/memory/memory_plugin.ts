// memory guest 插件（TypeScript，Process 域 gRPC）。
//
// 线契约：
//   {"op":"append","session_id":str,"messages":[Msg...]} → {"ok":true,"count":int}
//   {"op":"get","session_id":str,"limit"?:int}           → {"ok":true,"messages":[Msg...]}
//   {"op":"clear","session_id":str}                      → {"ok":true}
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
      default:
        return err(`unknown op: ${String(op)}`);
    }
  },
  destroy(): void {
    /* 无需清理：每次写操作已即时落盘 */
  },
};

serve(memoryPlugin);

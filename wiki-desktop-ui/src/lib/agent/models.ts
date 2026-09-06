/**
 * 模型列表统一入口 —— gateway 模式下必须反映网关真实可用模型
 * codex 的 `model/list` 返回的是其内置 GPT 模型目录，在 gateway 认证模式下
 * codex 实际请求的是 tunnel 服务端 LLM 网关（仅含用户配置的模型），若仍展示
 * 内置目录会导致用户选中不可用模型并在发送时报错。因此：
 * - gateway 模式：从服务端 `GET /api/llm/relay/models`（JWT 保护）拉取
 * - 其他模式（openai-key / chatgpt）：透传 codex `model/list`
 */

import { loadSyncConfig } from "@/api/server";
import { getToken } from "@/lib/server-auth";
import { listRelayModels } from "@/lib/ai-client";
import { modelList } from "./client";

export type ModelOption = { id: string; label: string };

export async function listAvailableModels(authMode: string): Promise<ModelOption[]> {
  if (authMode === "gateway") {
    const cfg = loadSyncConfig();
    const baseUrl = cfg?.baseUrl?.trim() ?? "";
    if (!baseUrl) {
      throw new Error("未连接服务器，请在设置中完成登录");
    }
    const token = getToken(baseUrl);
    if (!token) {
      throw new Error("未连接服务器，请在设置中完成登录");
    }
    const ids = await listRelayModels(baseUrl);
    return ids.map((id) => ({ id, label: id }));
  }

  const res = await modelList({});
  const data = (res as unknown as { data?: unknown })?.data;
  const arr = Array.isArray(data) ? data : [];
  return arr.map((raw) => {
    const m = raw as { id?: unknown; model?: unknown; displayName?: unknown };
    const rawId = (typeof m.model === "string" && m.model) || (typeof m.id === "string" ? m.id : "") || "";
    const id = String(rawId);
    const displayName = typeof m.displayName === "string" ? m.displayName : "";
    const label = displayName ? `${displayName} (${id})` : id;
    return { id, label };
  });
}

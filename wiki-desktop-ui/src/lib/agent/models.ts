/**
 * 模型列表统一入口 —— gateway 模式下必须反映网关真实可用模型
 * codex 的 `model/list` 返回的是其内置 GPT 模型目录，在 gateway 认证模式下
 * codex 实际请求的是 tunnel 服务端 LLM 网关（仅含用户配置的模型），若仍展示
 * 内置目录会导致用户选中不可用模型并在发送时报错。因此：
 * - gateway 模式：从服务端 `GET /api/llm/relay/models`（JWT 保护）拉取
 * - 其他模式（openai-key / chatgpt）：透传 codex `model/list`
 *
 * 注意：本模块不 import vendor 类型（只有 client.ts 可以），codex 原始字段
 * 用结构化松类型读取：
 * - `supportedReasoningEfforts: Array<{ reasoningEffort, description }>`
 *   （见 vendor `types/v2/ReasoningEffortOption.ts`）
 * - `defaultReasoningEffort: ReasoningEffort`（即 string，
 *   见 vendor `types/ReasoningEffort.ts`）
 * gateway 模式（relay 仅 id 列表）无上述元数据，`efforts` 为空数组。
 */

import { loadSyncConfig } from "@/api/server";
import { getToken } from "@/lib/server-auth";
import { listRelayModels } from "@/lib/ai-client";
import { modelList } from "./client";

/** 单个推理强度选项（由 vendor ReasoningEffortOption 映射而来） */
export type EffortOption = {
  /** codex turnStart 可接受的 effort 值（如 "low"/"medium"/"high"） */
  effort: string;
  /** vendor 附带的强度说明（可能为空） */
  description?: string;
};

export type ModelOption = {
  id: string;
  label: string;
  /** 可选推理强度列表；缺省/空 = 无强度菜单（gateway 模式无元数据） */
  efforts?: EffortOption[];
  /** 模型默认强度；null/缺省 = 无默认（跟随模型默认） */
  defaultEffort?: string | null;
};

type RawEffortEntry = {
  reasoningEffort?: unknown;
  description?: unknown;
};

type RawModel = {
  id?: unknown;
  model?: unknown;
  displayName?: unknown;
  supportedReasoningEfforts?: unknown;
  defaultReasoningEffort?: unknown;
};

/** codex model/list 原始条目 → ModelOption（含 efforts 元数据映射） */
function mapCodexModel(raw: unknown): ModelOption {
  const m = (raw ?? {}) as RawModel;
  const rawId = (typeof m.model === "string" && m.model) || (typeof m.id === "string" ? m.id : "") || "";
  const id = String(rawId);
  const displayName = typeof m.displayName === "string" ? m.displayName : "";
  const label = displayName ? `${displayName} (${id})` : id;

  const efforts: EffortOption[] = [];
  if (Array.isArray(m.supportedReasoningEfforts)) {
    for (const entry of m.supportedReasoningEfforts) {
      const e = (entry ?? {}) as RawEffortEntry;
      if (typeof e.reasoningEffort !== "string" || !e.reasoningEffort) continue;
      const opt: EffortOption = { effort: e.reasoningEffort };
      if (typeof e.description === "string" && e.description) {
        opt.description = e.description;
      }
      efforts.push(opt);
    }
  }
  const defaultEffort =
    typeof m.defaultReasoningEffort === "string" && m.defaultReasoningEffort
      ? m.defaultReasoningEffort
      : null;
  return { id, label, efforts, defaultEffort };
}

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
    // relay 仅返回 id 列表，无推理强度元数据
    return ids.map((id) => ({ id, label: id, efforts: [], defaultEffort: null }));
  }

  const res = await modelList({});
  const data = (res as unknown as { data?: unknown })?.data;
  const arr = Array.isArray(data) ? data : [];
  return arr.map((raw) => mapCodexModel(raw));
}

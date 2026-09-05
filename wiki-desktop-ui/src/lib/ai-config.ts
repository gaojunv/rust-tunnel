/**
 * AI 配置辅助 —— 统一读取 baseUrl + model，供 AiChatPanel 与选区工具条共用
 * 存储 key 与现有实现保持一致
 */
import { loadSyncConfig } from "@/api/server";

/** 模型选择持久化 key */
export const AI_MODEL_KEY = "wiki.ai.model.v1";

/** 读取已选模型（try/catch 兼容隐私模式） */
export function getAiModel(): string | null {
  try {
    return localStorage.getItem(AI_MODEL_KEY);
  } catch {
    return null;
  }
}

/** 保存已选模型 */
export function setAiModel(model: string): void {
  try {
    localStorage.setItem(AI_MODEL_KEY, model);
  } catch {
    // 忽略存储异常
  }
}

/**
 * 读取 AI 调用所需配置
 * - 无同步配置或未选模型时返回 null（调用方据此提示「打开设置」）
 */
export function getAiConfig(): { baseUrl: string; model: string } | null {
  const cfg = loadSyncConfig();
  if (!cfg?.baseUrl) return null;
  const model = getAiModel();
  if (!model) return null;
  return { baseUrl: cfg.baseUrl, model };
}

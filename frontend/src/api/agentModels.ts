import { listAllLlmModels, listLlmModelGroups, listLlmProviders } from './client';

export interface SelectableModel {
  /** 提交给后端的标识：模型为 alias||model_name，组为组名。 */
  id: string;
  /** 下拉展示名。 */
  label: string;
}

export interface AgentModelOptions {
  models: SelectableModel[];
  groups: SelectableModel[];
  /** llm_models.id → 可提交的模型标识（alias||model_name），仅启用项。用于把
   *  workspace 默认模型（存 model:<llm_models.id>）解析为前端可显示/提交的 id。
   *  可选：旧调用方/测试未提供时回退链条跳过 workspace 层。 */
  byModelId?: Record<string, string>;
  /** llm_model_groups.id → 组名，仅启用项（workspace 默认组 group:<id> 解析用）。 */
  byGroupId?: Record<string, string>;
}

/**
 * 聚合 Agent 可选模型：启用模型（alias||model_name）+ 启用模型组（组名）。
 * 后端 resolve_with_failover 对两者同一字段解析，无需区分提交类型。
 * 模型展示名改为「模型名（供应商名）」，不再用别名（别名只是内部标识）。
 */
export async function listAgentSelectableModels(): Promise<AgentModelOptions> {
  const [models, groups, providers] = await Promise.all([
    listAllLlmModels(),
    listLlmModelGroups(),
    listLlmProviders(),
  ]);
  const providerName = new Map(providers.map((p) => [p.id, p.name]));
  return {
    models: models
      .filter((m) => m.enabled)
      .map((m) => {
        const pname = m.provider_id ? providerName.get(m.provider_id) : undefined;
        const label = pname ? `${m.model_name}（${pname}）` : m.model_name;
        return { id: m.alias || m.model_name, label };
      }),
    groups: groups
      .filter((g) => g.enabled)
      .map((g) => ({ id: g.name, label: g.name })),
    byModelId: Object.fromEntries(
      models.filter((m) => m.enabled).map((m) => [m.id, m.alias || m.model_name]),
    ),
    byGroupId: Object.fromEntries(
      groups.filter((g) => g.enabled).map((g) => [g.id, g.name]),
    ),
  };
}

/**
 * 把 workspace 默认模型引用（`model:<id>` / `group:<id>` / 历史裸值）解析为前端
 * 可提交/显示的模型标识，与后端 `resolve_workspace_model_ref` 语义对齐（M11：
 * 前端模型解析链补齐 workspace 层，避免把全局默认/首个可用误当 workspace 默认）：
 * - `model:<id>` → byModelId[id]（不存在/禁用返回 ''，链条回退到下一层）；
 * - `group:<id>` → byGroupId[id]；
 * - 历史裸值：命中 byModelId[id] → 该模型标识；未命中 → 原样直通（可能是
 *   alias/model_name/组名，交给后端网关解析）。
 * 未配置 / 空 → ''。
 */
export function resolveWorkspaceModelRef(
  llmModelId: string | undefined,
  options: AgentModelOptions | undefined,
): string {
  if (!llmModelId) return '';
  const raw = llmModelId.trim();
  if (!raw) return '';
  if (raw.startsWith('model:')) {
    return options?.byModelId?.[raw.slice('model:'.length)] ?? '';
  }
  if (raw.startsWith('group:')) {
    return options?.byGroupId?.[raw.slice('group:'.length)] ?? '';
  }
  return options?.byModelId?.[raw] ?? raw;
}

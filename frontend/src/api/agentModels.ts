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
  };
}

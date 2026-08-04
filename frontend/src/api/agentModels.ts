import { listAllLlmModels, listLlmModelGroups } from './client';

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
 */
export async function listAgentSelectableModels(): Promise<AgentModelOptions> {
  const [models, groups] = await Promise.all([listAllLlmModels(), listLlmModelGroups()]);
  return {
    models: models
      .filter((m) => m.enabled)
      .map((m) => {
        const label = m.alias || m.model_name;
        return { id: label, label };
      }),
    groups: groups
      .filter((g) => g.enabled)
      .map((g) => ({ id: g.name, label: g.name })),
  };
}

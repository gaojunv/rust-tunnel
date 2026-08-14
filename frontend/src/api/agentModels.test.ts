import { describe, expect, it } from 'vitest';
import { resolveWorkspaceModelRef, type AgentModelOptions } from './agentModels';

const opts: AgentModelOptions = {
  models: [{ id: 'claude-opus-5', label: 'claude-opus-5（Anthropic）' }],
  groups: [{ id: 'fast-group', label: 'fast-group' }],
  byModelId: { 'model-1': 'claude-opus-5', 'model-2': 'disabled-model' },
  byGroupId: { 'group-1': 'fast-group', 'group-2': 'disabled-group' },
};

describe('resolveWorkspaceModelRef', () => {
  it('model:<id> 解析为可提交模型标识（alias||model_name）', () => {
    expect(resolveWorkspaceModelRef('model:model-1', opts)).toBe('claude-opus-5');
  });

  it('group:<id> 解析为组名', () => {
    expect(resolveWorkspaceModelRef('group:group-1', opts)).toBe('fast-group');
  });

  it('未配置 / 空串返回空（链条回退到下一层）', () => {
    expect(resolveWorkspaceModelRef(undefined, opts)).toBe('');
    expect(resolveWorkspaceModelRef('', opts)).toBe('');
    expect(resolveWorkspaceModelRef('   ', opts)).toBe('');
  });

  it('id 不存在/被禁用返回空（byModelId 只含启用项）', () => {
    expect(resolveWorkspaceModelRef('model:nope', opts)).toBe('');
  });

  it('历史裸值：命中 llm_models.id → 该模型标识', () => {
    expect(resolveWorkspaceModelRef('model-1', opts)).toBe('claude-opus-5');
  });

  it('历史裸值：未命中 → 原样直通（交后端网关解析）', () => {
    expect(resolveWorkspaceModelRef('my-alias', opts)).toBe('my-alias');
  });

  it('selectableModels 未提供 byModelId/byGroupId（旧 mock）时安全回退', () => {
    expect(resolveWorkspaceModelRef('model:model-1', { models: [], groups: [] })).toBe('');
    expect(resolveWorkspaceModelRef('raw-alias', { models: [], groups: [] })).toBe('raw-alias');
  });
});

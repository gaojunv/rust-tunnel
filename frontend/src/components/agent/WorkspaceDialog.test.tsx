// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import WorkspaceDialog from './WorkspaceDialog';
import type { AgentWorkspace } from '../../types';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

const api = vi.hoisted(() => ({
  createAgentWorkspace: vi.fn(),
  updateAgentWorkspace: vi.fn(),
}));

vi.mock('../../api/client', () => ({
  clientsApi: { list: vi.fn().mockResolvedValue([{ name: 'nas', online: true }]) },
  createAgentWorkspace: api.createAgentWorkspace,
  updateAgentWorkspace: api.updateAgentWorkspace,
  getApiErrorMessage: (err: unknown) => (err as Error)?.message ?? String(err),
  listAllLlmModels: vi.fn().mockResolvedValue([
    {
      id: 'm1',
      provider_id: 'p1',
      model_name: 'gpt-4o',
      alias: 'gpt-4o',
      tags: [],
      enabled: true,
      created_at: '',
      updated_at: '',
    },
    {
      id: 'm2',
      provider_id: 'p1',
      model_name: 'deepseek',
      alias: 'ds',
      tags: [],
      enabled: false,
      created_at: '',
      updated_at: '',
    },
  ]),
  listLlmModelGroups: vi.fn().mockResolvedValue([
    { id: 'g1', name: 'router', enabled: true, created_at: '', updated_at: '' },
    { id: 'g2', name: 'off', enabled: false, created_at: '', updated_at: '' },
  ]),
  listLlmProviders: vi.fn().mockResolvedValue([
    { id: 'p1', name: 'OpenAI', provider_type: 'openai', base_url: '', enabled: true, created_at: '', updated_at: '' },
  ]),
}));

const editingWs: AgentWorkspace = {
  id: 'w1',
  name: 'proj',
  client_id: 'nas',
  runtime_type: 'host',
  root_path: '/p',
  approval_mode: 'safe',
  system_prompt: null,
  agent_type: 'gemini',
  agent_path: '/opt/gemini',
  llm_model_id: 'm1',
  created_at: '',
  updated_at: '',
};

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const renderDialog = (editing?: AgentWorkspace) => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <WorkspaceDialog editing={editing} onClose={vi.fn()} onCreated={vi.fn()} />
    </QueryClientProvider>
  );
};

const selectEngine = (value: string) => {
  fireEvent.change(screen.getByLabelText('agent.agentEngine'), { target: { value } });
};

describe('WorkspaceDialog ACP config', () => {
  it('reveals agent path + LLM model when an ACP engine is chosen (host)', async () => {
    renderDialog();
    // 内置引擎（默认）：不显示 path / model 控件
    expect(screen.queryByLabelText('agent.workspaceLlmModel')).toBeNull();
    expect(screen.queryByPlaceholderText('agent.agentPathPlaceholder')).toBeNull();

    selectEngine('gemini');
    // path 输入框 + LLM 模型下拉出现；仅启用的模型/组可选（带类型前缀）
    expect(screen.getByPlaceholderText('agent.agentPathPlaceholder')).toBeTruthy();
    const modelSelect = screen.getByLabelText('agent.workspaceLlmModel') as HTMLSelectElement;
    await waitFor(() => {
      const values = Array.from(modelSelect.options).map((o) => o.value);
      expect(values).toContain('model:m1');
      expect(values).not.toContain('model:m2');
      expect(values).toContain('group:g1');
      expect(values).not.toContain('group:g2');
    });
    // 模型展示为「模型名（供应商名）」，不用别名
    const labels = Array.from(modelSelect.options).map((o) => o.text);
    expect(labels).toContain('gpt-4o（OpenAI）');
    // host 模式无 docker 提示
    expect(screen.queryByText('agent.acpDockerUnsupportedHint')).toBeNull();
  });

  it('shows docker-unsupported hint when engine chosen in docker runtime', async () => {
    renderDialog();
    fireEvent.click(screen.getByText('agent.runtimeDocker'));
    selectEngine('claude-code');
    expect(screen.getByText('agent.acpDockerUnsupportedHint')).toBeTruthy();
    // 切回内置引擎 → 提示消失
    selectEngine('');
    expect(screen.queryByText('agent.acpDockerUnsupportedHint')).toBeNull();
  });

  it('edit mode prefills ACP fields and submits them via PUT', async () => {
    renderDialog(editingWs);
    // 预填：引擎、路径、模型
    expect((screen.getByLabelText('agent.agentEngine') as HTMLSelectElement).value).toBe('gemini');
    expect(
      (screen.getByPlaceholderText('agent.agentPathPlaceholder') as HTMLInputElement).value,
    ).toBe('/opt/gemini');
    await waitFor(() => {
      expect((screen.getByLabelText('agent.workspaceLlmModel') as HTMLSelectElement).value).toBe(
        'model:m1',
      );
    });

    // 修改路径后保存（编辑模式按钮为 common.save）→ PUT 携带 ACP 字段
    fireEvent.change(screen.getByPlaceholderText('agent.agentPathPlaceholder'), {
      target: { value: '/usr/bin/gemini-acp' },
    });
    fireEvent.click(screen.getByText('common.save'));
    await waitFor(() => {
      expect(api.updateAgentWorkspace).toHaveBeenCalledWith('w1', {
        name: 'proj',
        root_path: '/p',
        system_prompt: '',
        approval_mode: 'safe',
        agent_type: 'gemini',
        agent_path: '/usr/bin/gemini-acp',
        llm_model_id: 'model:m1',
      });
    });
  });

  it('create mode submits ACP fields with the workspace', async () => {
    api.createAgentWorkspace.mockResolvedValue({ ...editingWs, id: 'w-new' });
    renderDialog();
    // 等待 clients 加载完成（client 下拉 enabled）后再交互
    await screen.findByRole('option', { name: 'nas' });
    fireEvent.change(screen.getByPlaceholderText('agent.namePlaceholder'), {
      target: { value: 'acp-proj' },
    });
    fireEvent.change(screen.getByLabelText('agent.client'), { target: { value: 'nas' } });
    fireEvent.change(screen.getByPlaceholderText('agent.rootPathPlaceholderHost'), {
      target: { value: '/workspace' },
    });
    selectEngine('gemini');
    fireEvent.change(screen.getByPlaceholderText('agent.agentPathPlaceholder'), {
      target: { value: '/opt/gemini' },
    });
    // 等待模型列表加载后选择
    await waitFor(() => {
      const modelSelect = screen.getByLabelText('agent.workspaceLlmModel') as HTMLSelectElement;
      expect(Array.from(modelSelect.options).map((o) => o.value)).toContain('model:m1');
    });
    fireEvent.change(screen.getByLabelText('agent.workspaceLlmModel'), {
      target: { value: 'model:m1' },
    });

    fireEvent.click(screen.getByText('agent.create'));
    await waitFor(() => {
      expect(api.createAgentWorkspace).toHaveBeenCalledWith({
        name: 'acp-proj',
        client_id: 'nas',
        runtime_type: 'host',
        root_path: '/workspace',
        docker_image: undefined,
        docker_container_id: undefined,
        agent_type: 'gemini',
        agent_path: '/opt/gemini',
        llm_model_id: 'model:m1',
      });
    });
  });

  it('create without ACP engine sends empty agent_type (built-in runner)', async () => {
    api.createAgentWorkspace.mockResolvedValue({ ...editingWs, id: 'w-new', agent_type: '' });
    renderDialog();
    // 等待 clients 加载完成（client 下拉 enabled）后再交互
    await screen.findByRole('option', { name: 'nas' });
    fireEvent.change(screen.getByPlaceholderText('agent.namePlaceholder'), {
      target: { value: 'plain' },
    });
    fireEvent.change(screen.getByLabelText('agent.client'), { target: { value: 'nas' } });
    fireEvent.change(screen.getByPlaceholderText('agent.rootPathPlaceholderHost'), {
      target: { value: '/p' },
    });
    fireEvent.click(screen.getByText('agent.create'));
    await waitFor(() => {
      expect(api.createAgentWorkspace).toHaveBeenCalledWith({
        name: 'plain',
        client_id: 'nas',
        runtime_type: 'host',
        root_path: '/p',
        docker_image: undefined,
        docker_container_id: undefined,
        agent_type: '',
      });
    });
  });
});

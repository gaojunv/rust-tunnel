// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import WorkspaceDialog, { parseOverrides, serializeOverrides } from './WorkspaceDialog';
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

// Radix Tabs 默认卸载 inactive Tab 的内容（未加 forceMount），交互前需先切到对应 Tab。
// Tabs 触发器带 aria-label，其可访问名即 i18n key 本身（mock 为 t: (k) => k）。
// RovingFocus 在 mousedown 上处理聚焦+激活（automatic 模式），jsdom 的 fireEvent.click
// 不带 focus 副作用不会触发切换，故先发 mouseDown 再 click，切换后内容挂载是异步的。
const clickTab = async (name: string) => {
  fireEvent.mouseDown(screen.getByRole('tab', { name }));
  fireEvent.click(screen.getByRole('tab', { name }));
  // 项目未引入 jest-dom，用原生属性断言 Tab 激活态
  await waitFor(() => {
    expect(screen.getByRole('tab', { name }).getAttribute('data-state')).toBe('active');
  });
};

const selectEngine = async (value: string) => {
  // 引擎下拉位于「引擎」Tab，先切换再交互
  await clickTab('agent.tabEngine');
  fireEvent.change(screen.getByLabelText('agent.agentEngine'), { target: { value } });
};

describe('WorkspaceDialog ACP config', () => {
  it('reveals agent path + LLM model when an ACP engine is chosen (host)', async () => {
    renderDialog();
    // 内置引擎（默认）：不显示 path / model 控件
    expect(screen.queryByLabelText('agent.workspaceLlmModel')).toBeNull();
    expect(screen.queryByPlaceholderText('agent.agentPathPlaceholder')).toBeNull();

    await selectEngine('gemini');
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
    await selectEngine('claude-code');
    expect(screen.getByText('agent.acpDockerUnsupportedHint')).toBeTruthy();
    // 切回内置引擎 → 提示消失
    await selectEngine('');
    expect(screen.queryByText('agent.acpDockerUnsupportedHint')).toBeNull();
  });

  it('edit mode prefills ACP fields and submits them via PUT', async () => {
    renderDialog(editingWs);
    // 引擎字段位于「引擎」Tab，先切换（Radix 默认卸载 inactive 内容）
    await clickTab('agent.tabEngine');
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
        github_owner: '',
        github_repo: '',
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
    await selectEngine('gemini');
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
        github_owner: '',
        github_repo: '',
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
        github_owner: '',
        github_repo: '',
      });
    });
  });
});

describe('parseOverrides / serializeOverrides', () => {
  it('parses stored JSON to rows; invalid/empty → []', () => {
    expect(parseOverrides('{"model":"sonnet","fast":"haiku"}')).toEqual([
      { key: 'model', value: 'sonnet' },
      { key: 'fast', value: 'haiku' },
    ]);
    expect(parseOverrides(undefined)).toEqual([]);
    expect(parseOverrides('')).toEqual([]);
    expect(parseOverrides('not-json')).toEqual([]);
    expect(parseOverrides('{}')).toEqual([]);
  });

  it('serializes rows to JSON, skipping empty keys', () => {
    expect(
      serializeOverrides([
        { key: 'model', value: 'sonnet' },
        { key: '', value: 'ignored' },
        { key: '  ', value: 'ignored' },
      ]),
    ).toBe('{"model":"sonnet"}');
  });

  it('returns undefined when no valid rows (caller decides {} vs omit)', () => {
    expect(serializeOverrides([])).toBeUndefined();
    expect(serializeOverrides([{ key: '', value: 'x' }])).toBeUndefined();
  });
});

describe('WorkspaceDialog config overrides UI', () => {
  it('编辑模式回填已有 overrides 行', async () => {
    renderDialog({ ...editingWs, agent_config_overrides: '{"model":"sonnet"}' });
    // overrides 行位于「引擎」Tab，先切换
    await clickTab('agent.tabEngine');
    expect((screen.getByLabelText('agent.configOverrides key 1') as HTMLInputElement).value).toBe(
      'model',
    );
    expect(
      (screen.getByLabelText('agent.configOverrides value 1') as HTMLInputElement).value,
    ).toBe('sonnet');
  });

  it('编辑模式：原有 overrides 被删空后提交发送 "{}" 清空', async () => {
    renderDialog({ ...editingWs, agent_config_overrides: '{"model":"sonnet"}' });
    // overrides 行位于「引擎」Tab，先切换
    await clickTab('agent.tabEngine');
    // 删除唯一一行
    fireEvent.click(screen.getByLabelText('agent.configOverrideRemove 1'));
    fireEvent.click(screen.getByText('common.save'));
    await waitFor(() => {
      expect(api.updateAgentWorkspace).toHaveBeenCalledWith(
        'w1',
        expect.objectContaining({ agent_config_overrides: '{}' }),
      );
    });
  });

  it('新建模式：填写行后提交发送 JSON', async () => {
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
    await selectEngine('claude-code');
    fireEvent.click(screen.getByText('agent.configOverrideAdd'));
    fireEvent.change(screen.getByLabelText('agent.configOverrides key 1'), {
      target: { value: 'model' },
    });
    fireEvent.change(screen.getByLabelText('agent.configOverrides value 1'), {
      target: { value: 'sonnet' },
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
        agent_type: 'claude-code',
        agent_config_overrides: '{"model":"sonnet"}',
        github_owner: '',
        github_repo: '',
      });
    });
  });

  it('新建模式：未填写 overrides 不发送该字段', async () => {
    api.createAgentWorkspace.mockResolvedValue({ ...editingWs, id: 'w-new' });
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
    await selectEngine('claude-code');
    fireEvent.click(screen.getByText('agent.create'));
    await waitFor(() => {
      expect(api.createAgentWorkspace).toHaveBeenCalled();
    });
    const body = api.createAgentWorkspace.mock.calls[0][0];
    expect(body).not.toHaveProperty('agent_config_overrides');
  });

  it('内置引擎（agent_type 空）不提交 overrides：防切回 ACP 时旧值复活', async () => {
    // 场景：工作区曾是 ACP 引擎且存有 overrides，编辑时切回内置 runner。
    // overrideRows 随引擎切换被隐藏但 state 仍持有旧值——必须按 agentType 门控，
    // 非 ACP 引擎不提交，否则旧值「复活」到后端。
    renderDialog({ ...editingWs, agent_config_overrides: '{"model":"sonnet"}' });
    // 引擎 Tab：切回内置引擎（空串）
    await clickTab('agent.tabEngine');
    fireEvent.change(screen.getByLabelText('agent.agentEngine'), { target: { value: '' } });
    fireEvent.click(screen.getByText('common.save'));
    await waitFor(() => {
      expect(api.updateAgentWorkspace).toHaveBeenCalledWith('w1', expect.anything());
    });
    const body = api.updateAgentWorkspace.mock.calls[0][1];
    expect(body.agent_type).toBe('');
    expect(body).not.toHaveProperty('agent_config_overrides');
  });
});

describe('WorkspaceDialog GitHub config', () => {
  const editingGithub: AgentWorkspace = {
    ...editingWs,
    github_token_set: true,
    github_owner: 'octo',
    github_repo: 'my-repo',
  };

  it('编辑模式：回填 owner/repo，token 占位提示已保存；留空不发送 token', async () => {
    renderDialog(editingGithub);
    await clickTab('agent.tabGithub');
    // owner/repo 回填
    expect((screen.getByPlaceholderText('agent.githubOwnerPlaceholder') as HTMLInputElement).value).toBe('octo');
    expect((screen.getByPlaceholderText('agent.githubRepoPlaceholder') as HTMLInputElement).value).toBe('my-repo');
    // token 密码框不回填明文，placeholder 提示已保存
    const token = screen.getByPlaceholderText('agent.githubTokenPlaceholder') as HTMLInputElement;
    expect(token.value).toBe('');
    fireEvent.click(screen.getByText('common.save'));
    await waitFor(() => {
      expect(api.updateAgentWorkspace).toHaveBeenCalledWith(
        'w1',
        expect.objectContaining({
          github_owner: 'octo',
          github_repo: 'my-repo',
        }),
      );
    });
    const body = api.updateAgentWorkspace.mock.calls[0][1];
    expect(body).not.toHaveProperty('github_token');
  });

  it('编辑模式：填写新 token 时发送 github_token', async () => {
    renderDialog(editingGithub);
    await clickTab('agent.tabGithub');
    fireEvent.change(screen.getByPlaceholderText('agent.githubTokenPlaceholder'), {
      target: { value: 'ghp_new_secret' },
    });
    fireEvent.click(screen.getByText('common.save'));
    await waitFor(() => {
      expect(api.updateAgentWorkspace).toHaveBeenCalledWith(
        'w1',
        expect.objectContaining({ github_token: 'ghp_new_secret' }),
      );
    });
  });

  it('新建模式：填写 owner/repo/token 后 create 携带全部 GitHub 字段', async () => {
    api.createAgentWorkspace.mockResolvedValue({ ...editingWs, id: 'w-new' });
    renderDialog();
    await screen.findByRole('option', { name: 'nas' });
    fireEvent.change(screen.getByPlaceholderText('agent.namePlaceholder'), {
      target: { value: 'gh-proj' },
    });
    fireEvent.change(screen.getByLabelText('agent.client'), { target: { value: 'nas' } });
    fireEvent.change(screen.getByPlaceholderText('agent.rootPathPlaceholderHost'), {
      target: { value: '/p' },
    });
    await clickTab('agent.tabGithub');
    fireEvent.change(screen.getByPlaceholderText('agent.githubOwnerPlaceholder'), {
      target: { value: 'octo' },
    });
    fireEvent.change(screen.getByPlaceholderText('agent.githubRepoPlaceholder'), {
      target: { value: 'my-repo' },
    });
    fireEvent.change(screen.getByPlaceholderText('agent.githubTokenPlaceholderEmpty'), {
      target: { value: 'ghp_create' },
    });
    fireEvent.click(screen.getByText('agent.create'));
    await waitFor(() => {
      expect(api.createAgentWorkspace).toHaveBeenCalledWith(
        expect.objectContaining({
          github_owner: 'octo',
          github_repo: 'my-repo',
          github_token: 'ghp_create',
        }),
      );
    });
  });

  it('新建模式：token 留空时 create 不发送 github_token', async () => {
    api.createAgentWorkspace.mockResolvedValue({ ...editingWs, id: 'w-new' });
    renderDialog();
    await screen.findByRole('option', { name: 'nas' });
    fireEvent.change(screen.getByPlaceholderText('agent.namePlaceholder'), {
      target: { value: 'plain-gh' },
    });
    fireEvent.change(screen.getByLabelText('agent.client'), { target: { value: 'nas' } });
    fireEvent.change(screen.getByPlaceholderText('agent.rootPathPlaceholderHost'), {
      target: { value: '/p' },
    });
    await clickTab('agent.tabGithub');
    fireEvent.change(screen.getByPlaceholderText('agent.githubOwnerPlaceholder'), {
      target: { value: 'octo' },
    });
    fireEvent.click(screen.getByText('agent.create'));
    await waitFor(() => {
      expect(api.createAgentWorkspace).toHaveBeenCalled();
    });
    const body = api.createAgentWorkspace.mock.calls[0][0];
    expect(body).not.toHaveProperty('github_token');
  });
});

// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import SessionSettingsMenu from './SessionSettingsMenu';
import { listAgentSelectableModels } from '../../api/agentModels';
import type { AgentModelOptions } from '../../api/agentModels';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({
    t: (k: string, opts?: Record<string, string>) => {
      if (k === 'agent.tierModelLabel' && opts) return `${opts.tier} · ${opts.model}`;
      return k;
    },
  }),
}));

vi.mock('../../api/agentModels', () => ({
  listAgentSelectableModels: vi.fn().mockResolvedValue({
    models: [{ id: 'deepseek-chat', label: 'deepseek-chat' }],
    groups: [],
  }),
  resolveWorkspaceModelRef: vi.fn((ref: string, data: AgentModelOptions | undefined) => {
    if (!ref) return '';
    const raw = ref.trim();
    if (raw.startsWith('model:')) return data?.byModelId?.[raw.slice('model:'.length)] ?? '';
    if (raw.startsWith('group:')) return data?.byGroupId?.[raw.slice('group:'.length)] ?? '';
    return data?.byModelId?.[raw] ?? raw;
  }),
}));

vi.mock('../../api/hooks', () => ({
  useRoles: () => ({ data: { roles: [] } }),
  useUpdateSessionRole: () => ({ mutate: vi.fn() }),
}));

afterEach(cleanup);

const renderMenu = (props: Partial<Parameters<typeof SessionSettingsMenu>[0]> = {}) => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <SessionSettingsMenu
        model="deepseek-chat"
        onModelChange={vi.fn()}
        configOptions={[]}
        onConfigChange={vi.fn()}
        {...props}
      />
    </QueryClientProvider>
  );
};

const openMenu = async () => {
  const trigger = screen.getByRole('button', { name: 'agent.sessionSettings' });
  fireEvent.pointerDown(trigger);
  fireEvent.click(trigger);
  await screen.findByPlaceholderText('agent.searchModels');
};

describe('SessionSettingsMenu', () => {
  it('shows current model label on the trigger', async () => {
    renderMenu();
    expect(await screen.findByText('deepseek-chat')).toBeTruthy();
  });

  it('renders generic config options passed in (mode/effort filtered upstream)', async () => {
    renderMenu({
      configOptions: [
        { id: 'fast', name: 'Fast', type: 'boolean', currentBool: true, currentValue: 'true' },
      ],
    });
    expect(await screen.findByText('deepseek-chat')).toBeTruthy();
  });

  it('opens a flat searchable model list at the top of the menu (no nested submenu)', async () => {
    renderMenu();
    await openMenu();
    const item = await screen.findByRole('menuitem', { name: 'deepseek-chat' });
    expect(item).toBeTruthy();
    expect(screen.getByText('agent.model')).toBeTruthy();
  });

  it('shows no-results message when model search matches nothing', async () => {
    renderMenu();
    await openMenu();
    fireEvent.change(screen.getByPlaceholderText('agent.searchModels'), {
      target: { value: 'no-such-model' },
    });
    expect(await screen.findByText('agent.noModelsFound')).toBeTruthy();
    expect(screen.queryByRole('menuitem', { name: 'deepseek-chat' })).toBeNull();
  });

  it('shows tier-mapped model when agentType is claude-code and currentValue is sonnet', async () => {
    vi.mocked(listAgentSelectableModels).mockResolvedValue({
      models: [
        { id: 'deepseek-chat', label: 'deepseek-chat' },
        { id: 'my-sonnet-alias', label: 'Claude Sonnet（Anthropic）' },
      ],
      groups: [],
      byModelId: {},
      byGroupId: {},
    });
    renderMenu({
      agentType: 'claude-code',
      claudeTierModels: '{"sonnet":"my-sonnet-alias"}',
      configOptions: [
        { id: 'model', name: 'Model', category: 'model', type: 'select', currentValue: 'sonnet', options: [] },
      ],
    });
    expect(await screen.findByText('Sonnet · Claude Sonnet（Anthropic）')).toBeTruthy();
  });

  it('falls back to session chain label when tier is unmapped (opus not in map)', async () => {
    vi.mocked(listAgentSelectableModels).mockResolvedValue({
      models: [
        { id: 'deepseek-chat', label: 'deepseek-chat' },
        { id: 'my-sonnet-alias', label: 'Claude Sonnet（Anthropic）' },
      ],
      groups: [],
      byModelId: {},
      byGroupId: {},
    });
    renderMenu({
      model: 'deepseek-chat',
      agentType: 'claude-code',
      claudeTierModels: '{"sonnet":"my-sonnet-alias"}',
      configOptions: [
        { id: 'model', name: 'Model', category: 'model', type: 'select', currentValue: 'opus', options: [] },
      ],
    });
    expect(await screen.findByText('deepseek-chat')).toBeTruthy();
    expect(screen.queryByText(/Sonnet ·/)).toBeNull();
  });

  it('shows direct model label when currentValue equals a selectable model id (passthrough)', async () => {
    vi.mocked(listAgentSelectableModels).mockResolvedValue({
      models: [
        { id: 'deepseek-chat', label: 'DeepSeek Chat' },
        { id: 'other-model', label: 'Other Model' },
      ],
      groups: [],
      byModelId: {},
      byGroupId: {},
    });
    renderMenu({
      model: 'other-model',
      agentType: null,
      claudeTierModels: null,
      configOptions: [
        { id: 'model', name: 'Model', category: 'model', type: 'select', currentValue: 'deepseek-chat', options: [] },
      ],
    });
    expect(await screen.findByText('DeepSeek Chat')).toBeTruthy();
  });

  it('keeps current behavior when no category=model option', async () => {
    vi.mocked(listAgentSelectableModels).mockResolvedValue({
      models: [{ id: 'deepseek-chat', label: 'deepseek-chat' }],
      groups: [],
      byModelId: {},
      byGroupId: {},
    });
    renderMenu({
      model: 'deepseek-chat',
      configOptions: [
        { id: 'fast', name: 'Fast', type: 'boolean', currentBool: true, currentValue: 'true' },
      ],
    });
    expect(await screen.findByText('deepseek-chat')).toBeTruthy();
  });

  it('treats default as sonnet tier when tier mapping contains sonnet', async () => {
    vi.mocked(listAgentSelectableModels).mockResolvedValue({
      models: [
        { id: 'deepseek-chat', label: 'deepseek-chat' },
        { id: 'my-sonnet-alias', label: 'Claude Sonnet（Anthropic）' },
      ],
      groups: [],
      byModelId: {},
      byGroupId: {},
    });
    renderMenu({
      agentType: 'claude-code',
      claudeTierModels: '{"sonnet":"my-sonnet-alias"}',
      configOptions: [
        { id: 'model', name: 'Model', category: 'model', type: 'select', currentValue: 'default', options: [] },
      ],
    });
    expect(await screen.findByText('Sonnet · Claude Sonnet（Anthropic）')).toBeTruthy();
  });

  it('shows tier active hint inside the menu when tier mapping is active', async () => {
    vi.mocked(listAgentSelectableModels).mockResolvedValue({
      models: [{ id: 'my-sonnet-alias', label: 'Claude Sonnet（Anthropic）' }],
      groups: [],
      byModelId: {},
      byGroupId: {},
    });
    renderMenu({
      agentType: 'claude-code',
      claudeTierModels: '{"sonnet":"my-sonnet-alias"}',
      configOptions: [
        { id: 'model', name: 'Model', category: 'model', type: 'select', currentValue: 'sonnet', options: [] },
      ],
    });
    await openMenu();
    expect(screen.getByText('agent.tierModelActiveHint')).toBeTruthy();
  });

  it('seeds tier model from persisted configState when WS snapshot absent (page switch)', async () => {
    vi.mocked(listAgentSelectableModels).mockResolvedValue({
      models: [{ id: 'my-opus-alias', label: 'Claude Opus（Anthropic）' }],
      groups: [],
      byModelId: {},
      byGroupId: {},
    });
    // 切页/刷新后：ACP 进程已回收，configOptions 为空（WS 快照未达），但
    // config_state 持久化过 model=opus —— 显示种子应显示映射模型而非默认。
    renderMenu({
      model: 'deepseek-chat',
      agentType: 'claude-code',
      claudeTierModels: '{"opus":"my-opus-alias"}',
      configOptions: [],
      configState: '{"model":"opus"}',
    });
    expect(await screen.findByText('Opus · Claude Opus（Anthropic）')).toBeTruthy();
  });

  it('prefers live WS snapshot over persisted configState when both present', async () => {
    vi.mocked(listAgentSelectableModels).mockResolvedValue({
      models: [
        { id: 'my-sonnet-alias', label: 'Claude Sonnet（Anthropic）' },
        { id: 'my-opus-alias', label: 'Claude Opus（Anthropic）' },
      ],
      groups: [],
      byModelId: {},
      byGroupId: {},
    });
    // WS 快照（进程活着/回放后）优先于持久化种子——config_state 可能滞后于在途切换。
    renderMenu({
      agentType: 'claude-code',
      claudeTierModels: '{"opus":"my-opus-alias","sonnet":"my-sonnet-alias"}',
      configOptions: [
        { id: 'model', name: 'Model', category: 'model', type: 'select', currentValue: 'sonnet', options: [] },
      ],
      configState: '{"model":"opus"}',
    });
    expect(await screen.findByText('Sonnet · Claude Sonnet（Anthropic）')).toBeTruthy();
    expect(screen.queryByText(/Opus ·/)).toBeNull();
  });
});

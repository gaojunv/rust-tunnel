import { useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Button } from '@/components/ui/button';
import { Loader2 } from 'lucide-react';
import {
  clientsApi,
  createAgentWorkspace,
  getApiErrorMessage,
  listAllLlmModels,
  listLlmModelGroups,
  listLlmProviders,
  updateAgentWorkspace,
} from '@/api/client';
import type {
  AgentWorkspace,
  Client,
  LlmModel,
  LlmModelGroup,
  LlmProvider,
} from '@/types';

/** 历史裸值（model uuid / alias / 组名）归一化为带类型前缀的引用；已带前缀原样返回。 */
const normalizeLlmRef = (raw?: string): string => {
  if (!raw) return '';
  if (raw.startsWith('model:') || raw.startsWith('group:')) return raw;
  return `model:${raw}`;
};

export interface OverrideRow {
  key: string;
  value: string;
}

/** 解析存储的 overrides JSON 为编辑行；非法/空 → []。 */
export const parseOverrides = (raw?: string): OverrideRow[] => {
  if (!raw) return [];
  try {
    const parsed: unknown = JSON.parse(raw);
    if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) return [];
    return Object.entries(parsed as Record<string, unknown>)
      .filter(([, v]) => typeof v === 'string')
      .map(([key, value]) => ({ key, value: value as string }));
  } catch {
    return [];
  }
};

/** 过滤空 key 行序列化为 JSON；无有效行 → undefined（调用方决定省略还是 "{}" 清空）。 */
export const serializeOverrides = (rows: OverrideRow[]): string | undefined => {
  const obj: Record<string, string> = {};
  for (const row of rows) {
    const key = row.key.trim();
    if (key !== '') obj[key] = row.value;
  }
  return Object.keys(obj).length > 0 ? JSON.stringify(obj) : undefined;
};

interface Props {
  /** 传入则为编辑模式：仅可改 name/root_path/system_prompt/approval_mode 及 ACP 字段（client/运行时不可变） */
  editing?: AgentWorkspace;
  onClose: () => void;
  onCreated: (w: AgentWorkspace) => void;
}

export default function WorkspaceDialog({ editing, onClose, onCreated }: Props) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const { data: clients, isLoading } = useQuery<Client[]>({
    queryKey: ['clients'],
    queryFn: clientsApi.list,
  });
  // LLM 模型下拉数据源：workspace llm_model_id 存「model:<id> / group:<id>」带类型
  // 前缀的引用，需用全量列表（而非 alias 名），并映射 provider_id → 供应商名。
  const { data: models } = useQuery<LlmModel[]>({
    queryKey: ['llm-models', 'all'],
    queryFn: listAllLlmModels,
  });
  const { data: groups } = useQuery<LlmModelGroup[]>({
    queryKey: ['llm-model-groups', 'all'],
    queryFn: listLlmModelGroups,
  });
  const { data: providers } = useQuery<LlmProvider[]>({
    queryKey: ['llm-providers'],
    queryFn: listLlmProviders,
  });

  const [name, setName] = useState(editing?.name ?? '');
  const [clientId, setClientId] = useState(editing?.client_id ?? '');
  const [runtimeType, setRuntimeType] = useState<'host' | 'docker'>(editing?.runtime_type ?? 'host');
  const [rootPath, setRootPath] = useState(editing?.root_path ?? '');
  const [dockerImage, setDockerImage] = useState(editing?.docker_image ?? '');
  const [dockerContainerId, setDockerContainerId] = useState(editing?.docker_container_id ?? '');
  const [approvalMode, setApprovalMode] = useState<'safe' | 'auto_write' | 'full_auto'>(
    editing?.approval_mode ?? 'safe',
  );
  const [systemPrompt, setSystemPrompt] = useState(editing?.system_prompt ?? '');
  // ACP 远程 agent 引擎：空串 = 内置 runner；非空 = gemini/claude-code/opencode
  const [agentType, setAgentType] = useState<AgentWorkspace['agent_type']>(
    editing?.agent_type ?? '',
  );
  const [agentPath, setAgentPath] = useState(editing?.agent_path ?? '');
  // 历史库中 llm_model_id 可能是裸 uuid，编辑时归一化为 `model:<id>` 以匹配下拉选项
  const [llmModelId, setLlmModelId] = useState(normalizeLlmRef(editing?.llm_model_id));
  const [overrideRows, setOverrideRows] = useState<OverrideRow[]>(
    parseOverrides(editing?.agent_config_overrides),
  );
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // provider_id → 供应商名（模型下拉展示「模型名（供应商名）」）
  const providerName = new Map((providers ?? []).map((p) => [p.id, p.name]));
  // 新建流程 create 成功后、补写 PUT 失败时记录已创建的工作区：用户点「创建」重试
  // 时不再重复 create（只重试补写），避免生成重复工作区。
  const createdRef = useRef<AgentWorkspace | null>(null);

  const canSubmit = editing
    ? name.trim() !== '' && rootPath.trim() !== ''
    : name.trim() !== '' &&
      clientId !== '' &&
      rootPath.trim() !== '' &&
      (runtimeType === 'host' || (dockerImage.trim() !== '' && dockerContainerId.trim() !== ''));

  // 序列化 overrides：有有效行 → JSON；无有效行且原记录有值（编辑模式）→ "{}" 清空；
  // 否则 undefined（不发送该字段，后端保持原值）。
  const overridesPayload = (): string | undefined =>
    serializeOverrides(overrideRows) ??
    (editing?.agent_config_overrides ? '{}' : undefined);

  const submit = async () => {
    if (!canSubmit || submitting) return;
    setSubmitting(true);
    setError(null);
    try {
      let w: AgentWorkspace;
      if (editing) {
        // 编辑：PUT 必须带上完整 name/root_path/system_prompt（空串即清除，缺省会被
        // 后端当作「未设置」而不是「清空」，形成缺省即清除陷阱）。agent_type 总是发送
        // （空串 = 内置 runner）；agent_path/llm_model_id 仅非空时发送（后端 None=保持
        // 原值，本迭代不支持清空）。
        await updateAgentWorkspace(editing.id, {
          name: name.trim(),
          root_path: rootPath.trim(),
          system_prompt: systemPrompt.trim(),
          approval_mode: approvalMode,
          agent_type: agentType,
          ...(agentPath.trim() !== '' ? { agent_path: agentPath.trim() } : {}),
          ...(llmModelId !== '' ? { llm_model_id: llmModelId } : {}),
          ...(overridesPayload() !== undefined
            ? { agent_config_overrides: overridesPayload() }
            : {}),
        });
        w = {
          ...editing,
          name: name.trim(),
          root_path: rootPath.trim(),
          system_prompt: systemPrompt.trim() || null,
          approval_mode: approvalMode,
          agent_type: agentType,
          agent_path: agentPath.trim() !== '' ? agentPath.trim() : undefined,
          llm_model_id: llmModelId !== '' ? llmModelId : undefined,
          agent_config_overrides: overridesPayload(),
        };
      } else {
        w = createdRef.current ?? (await createAgentWorkspace({
          name: name.trim(),
          client_id: clientId,
          runtime_type: runtimeType,
          root_path: rootPath.trim(),
          docker_image: runtimeType === 'docker' ? dockerImage.trim() : undefined,
          docker_container_id: runtimeType === 'docker' ? dockerContainerId.trim() : undefined,
          agent_type: agentType,
          ...(agentPath.trim() !== '' ? { agent_path: agentPath.trim() } : {}),
          ...(llmModelId !== '' ? { llm_model_id: llmModelId } : {}),
          ...(overridesPayload() !== undefined
            ? { agent_config_overrides: overridesPayload() }
            : {}),
        }));
        createdRef.current = w;
        // 后端 create 不含 system_prompt/approval_mode 字段（仅在 PUT 支持），
        // 用户在新建对话框设置的非默认值需创建成功后经 PUT 补写，否则静默丢失。
        const trimmedPrompt = systemPrompt.trim();
        if (trimmedPrompt !== '' || approvalMode !== 'safe') {
          try {
            await updateAgentWorkspace(w.id, {
              name: w.name,
              root_path: w.root_path,
              system_prompt: trimmedPrompt || undefined,
              approval_mode: approvalMode,
            });
          } catch (err) {
            // 工作区已创建成功，仅设置未落库：不阻断创建，但错误必须在对话框 error
            // 区域可见（用户可点「创建」重试补写，createdRef 保证不重复 create）。
            setError(getApiErrorMessage(err));
            setSubmitting(false);
            return;
          }
        }
      }
      // 先补写设置再刷新列表缓存，避免 refetch 返回未含设置的旧数据
      await queryClient.invalidateQueries({ queryKey: ['agent-workspaces'] });
      onCreated(w);
    } catch (err) {
      setError(getApiErrorMessage(err));
      setSubmitting(false);
    }
  };

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>{editing ? t('agent.editWorkspace') : t('agent.newWorkspace')}</DialogTitle>
        </DialogHeader>
        <div className="space-y-4">
          <div className="space-y-2">
            <Label>{t('agent.name')}</Label>
            <Input value={name} onChange={(e) => setName(e.target.value)} placeholder={t('agent.namePlaceholder')} />
          </div>
          {!editing && (
            <>
              <div className="space-y-2">
                <Label>{t('agent.client')}</Label>
                <select
                  value={clientId}
                  onChange={(e) => setClientId(e.target.value)}
                  disabled={isLoading}
                  aria-label={t('agent.client')}
                  className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm disabled:cursor-not-allowed disabled:opacity-50"
                >
                  <option value="">
                    {isLoading ? t('common.loading') : t('agent.selectClient')}
                  </option>
                  {(clients ?? []).map((c) => (
                    <option key={c.name} value={c.name}>
                      {c.name}
                      {c.online ? '' : `（${t('common.status.offline')}）`}
                    </option>
                  ))}
                </select>
              </div>
              <div className="space-y-2">
                <Label>{t('agent.runtimeType')}</Label>
                <div className="flex gap-4">
                  <label className="flex items-center gap-2 text-sm">
                    <input
                      type="radio"
                      checked={runtimeType === 'host'}
                      onChange={() => setRuntimeType('host')}
                    />
                    {t('agent.runtimeHost')}
                  </label>
                  <label className="flex items-center gap-2 text-sm">
                    <input
                      type="radio"
                      checked={runtimeType === 'docker'}
                      onChange={() => setRuntimeType('docker')}
                    />
                    {t('agent.runtimeDocker')}
                  </label>
                </div>
              </div>
            </>
          )}
          <div className="space-y-2">
            <Label>{t('agent.rootPath')}</Label>
            <Input
              value={rootPath}
              onChange={(e) => setRootPath(e.target.value)}
              placeholder={runtimeType === 'host' ? t('agent.rootPathPlaceholderHost') : t('agent.rootPathPlaceholderDocker')}
            />
          </div>
          <div className="space-y-2">
            <Label>{t('agent.agentEngine')}</Label>
            <select
              value={agentType}
              onChange={(e) => setAgentType(e.target.value as AgentWorkspace['agent_type'])}
              aria-label={t('agent.agentEngine')}
              className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
            >
              <option value="">{t('agent.agentEngineBuiltin')}</option>
              <option value="gemini">Gemini</option>
              <option value="claude-code">Claude Code</option>
              <option value="opencode">OpenCode</option>
            </select>
            {runtimeType === 'docker' && agentType !== '' && (
              <p className="text-xs text-destructive">{t('agent.acpDockerUnsupportedHint')}</p>
            )}
          </div>
          {agentType !== '' && (
            <>
              <div className="space-y-2">
                <Label>{t('agent.agentPath')}</Label>
                <Input
                  value={agentPath}
                  onChange={(e) => setAgentPath(e.target.value)}
                  placeholder={t('agent.agentPathPlaceholder')}
                />
              </div>
              <div className="space-y-2">
                <Label>{t('agent.workspaceLlmModel')}</Label>
                <select
                  value={llmModelId}
                  onChange={(e) => setLlmModelId(e.target.value)}
                  aria-label={t('agent.workspaceLlmModel')}
                  className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
                >
                  <option value="">{t('agent.selectModel')}</option>
                  <optgroup label={t('agent.model')}>
                    {(models ?? [])
                      .filter((m) => m.enabled)
                      .map((m) => {
                        const pname = m.provider_id
                          ? providerName.get(m.provider_id)
                          : undefined;
                        const label = pname
                          ? `${m.model_name}（${pname}）`
                          : m.model_name;
                        return (
                          <option key={m.id} value={`model:${m.id}`}>
                            {label}
                          </option>
                        );
                      })}
                  </optgroup>
                  {(groups ?? []).some((g) => g.enabled) && (
                    <optgroup label={t('agent.modelGroups')}>
                      {(groups ?? [])
                        .filter((g) => g.enabled)
                        .map((g) => (
                          <option key={g.id} value={`group:${g.id}`}>
                            {g.name}
                          </option>
                        ))}
                    </optgroup>
                  )}
                </select>
              </div>
              <div className="space-y-2">
                <Label>{t('agent.configOverrides')}</Label>
                {overrideRows.map((row, i) => (
                  <div key={i} className="flex items-center gap-2">
                    <Input
                      value={row.key}
                      onChange={(e) =>
                        setOverrideRows((rows) =>
                          rows.map((r, j) => (j === i ? { ...r, key: e.target.value } : r)),
                        )
                      }
                      placeholder={t('agent.configOverrideKeyPlaceholder')}
                      aria-label={`${t('agent.configOverrides')} key ${i + 1}`}
                      className="w-40"
                    />
                    <Input
                      value={row.value}
                      onChange={(e) =>
                        setOverrideRows((rows) =>
                          rows.map((r, j) => (j === i ? { ...r, value: e.target.value } : r)),
                        )
                      }
                      placeholder={t('agent.configOverrideValuePlaceholder')}
                      aria-label={`${t('agent.configOverrides')} value ${i + 1}`}
                    />
                    <Button
                      type="button"
                      variant="ghost"
                      size="sm"
                      aria-label={`${t('agent.configOverrideRemove')} ${i + 1}`}
                      onClick={() => setOverrideRows((rows) => rows.filter((_, j) => j !== i))}
                    >
                      ×
                    </Button>
                  </div>
                ))}
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={() => setOverrideRows((rows) => [...rows, { key: '', value: '' }])}
                >
                  {t('agent.configOverrideAdd')}
                </Button>
                <p className="text-xs text-muted-foreground">{t('agent.configOverridesHint')}</p>
              </div>
            </>
          )}
          <div className="space-y-2">
            <Label>{t('agent.approvalMode')}</Label>
            <div className="space-y-1.5">
              {(['safe', 'auto_write', 'full_auto'] as const).map((m) => (
                <label key={m} className="flex items-start gap-2 text-sm">
                  <input type="radio" checked={approvalMode === m} onChange={() => setApprovalMode(m)} className="mt-1" />
                  <span>
                    <span className="font-medium">{t(`agent.approvalMode_${m}`)}</span>
                    <span className="ml-1.5 text-xs text-muted-foreground">{t(`agent.approvalModeHint_${m}`)}</span>
                  </span>
                </label>
              ))}
            </div>
          </div>
          <div className="space-y-2">
            <Label>{t('agent.systemPrompt')}</Label>
            <textarea
              value={systemPrompt}
              onChange={(e) => setSystemPrompt(e.target.value)}
              placeholder={t('agent.systemPromptPlaceholder')}
              rows={3}
              className="w-full resize-y rounded-md border border-input bg-background px-3 py-2 text-sm"
            />
          </div>
          {!editing && runtimeType === 'docker' && (
            <div className="space-y-2">
              <Label>{t('agent.dockerImage')}</Label>
              <Input
                value={dockerImage}
                onChange={(e) => setDockerImage(e.target.value)}
                placeholder={t('agent.dockerImagePlaceholder')}
              />
              <Label>{t('agent.dockerContainerId')}</Label>
              <Input
                value={dockerContainerId}
                onChange={(e) => setDockerContainerId(e.target.value)}
                placeholder={t('agent.dockerContainerIdPlaceholder')}
              />
              <p className="text-xs text-muted-foreground">
                {t('agent.dockerContainerIdHint')}
              </p>
            </div>
          )}
        </div>
        {error && <p className="text-sm text-destructive">{error}</p>}
        <DialogFooter>
          <Button variant="outline" onClick={onClose} disabled={submitting}>
            {t('common.cancel')}
          </Button>
          <Button onClick={submit} disabled={!canSubmit || submitting}>
            {submitting && <Loader2 className="mr-1 h-4 w-4 animate-spin" />}
            {editing ? t('common.save') : t('agent.create')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

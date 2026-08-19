import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery } from '@tanstack/react-query';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter } from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Button } from '@/components/ui/button';
import { Loader2 } from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import { useCreateRole, useUpdateRole, useClients, useAgentWorkspaces } from '@/api/hooks';
import { listAgentSelectableModels } from '@/api/agentModels';
import type { AgentRole, AgentRoleMode, AgentRoleScope, CreateRoleRequest, UpdateRoleRequest } from '@/types';

interface Props {
  open: boolean;
  onClose: () => void;
  role?: AgentRole | null;
}

const TOOLS_GROUPED = [
  {
    label: 'Shell & File',
    tools: ['shell', 'read_file', 'write_file', 'patch_file', 'edit_file', 'list_dir', 'search'],
  },
  {
    label: 'Git',
    tools: ['git_status', 'git_diff', 'git_log', 'git_show', 'git_branch', 'git_commit', 'git_push', 'git_stage', 'git_unstage', 'git_checkout', 'git_pull', 'git_revert', 'git_reset', 'git_stash'],
  },
  {
    label: 'Code & Agent',
    tools: ['code_outline', 'read_symbol', 'task', 'todo_write'],
  },
];

const MODE_OPTIONS = [
  { value: 'subagent' as const, labelKey: 'role.mode_subagent' as const },
  { value: 'primary' as const, labelKey: 'role.mode_primary' as const },
  { value: 'all' as const, labelKey: 'role.mode_all' as const },
];

const SCOPE_OPTIONS = [
  { value: 'global' as const, labelKey: 'role.scope_global' as const },
  { value: 'client' as const, labelKey: 'role.scope_client' as const },
  { value: 'workspace' as const, labelKey: 'role.scope_workspace' as const },
];

export default function RoleDialog({ open, onClose, role = null }: Props) {
  const { t } = useTranslation();
  const createMutation = useCreateRole();
  const updateMutation = useUpdateRole();
  const { data: clients } = useClients();
  const { data: workspaces } = useAgentWorkspaces();
  const { data: modelOptions } = useQuery({
    queryKey: ['agent-selectable-models'],
    queryFn: listAgentSelectableModels,
    staleTime: 60_000,
  });

  const isEdit = !!role;
  const isBuiltin = role?.is_builtin ?? false;

  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [systemPrompt, setSystemPrompt] = useState('');
  const [toolsAllow, setToolsAllow] = useState<string[] | null>(null);
  const [toolsDeny, setToolsDeny] = useState<string[] | null>(null);
  const [modelOverride, setModelOverride] = useState('');
  const [mode, setMode] = useState<AgentRoleMode>('all');
  const [scope, setScope] = useState<AgentRoleScope>('global');
  const [clientId, setClientId] = useState('');
  const [workspaceId, setWorkspaceId] = useState('');
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [toolsTab, setToolsTab] = useState<'allow' | 'deny'>('allow');

  const initRef = useRef(false);
  useEffect(() => {
    if (!open) {
      initRef.current = false;
      return;
    }
    if (initRef.current) return;
    initRef.current = true;
    if (role) {
      setName(role.name);
      setDescription(role.description);
      setSystemPrompt(role.system_prompt);
      setToolsAllow(role.tools_allow);
      setToolsDeny(role.tools_deny);
      setModelOverride(role.model_override ?? '');
      setMode(role.mode);
      setScope(role.scope_type);
      setClientId(role.client_id);
      setWorkspaceId(role.workspace_id);
    } else {
      setName('');
      setDescription('');
      setSystemPrompt('');
      setToolsAllow(null);
      setToolsDeny(null);
      setModelOverride('');
      setMode('all');
      setScope('global');
      setClientId('');
      setWorkspaceId('');
    }
    setSubmitError(null);
    setToolsTab('allow');
  }, [open, role]);

  const changeScope = (s: AgentRoleScope) => {
    setScope(s);
    setClientId('');
    setWorkspaceId('');
  };

  const toggleTool = (toolName: string, target: 'allow' | 'deny') => {
    if (target === 'allow') {
      setToolsAllow((prev) => {
        const cur = prev ?? [];
        if (cur.includes(toolName)) return cur.filter((t) => t !== toolName);
        return [...cur, toolName];
      });
    } else {
      setToolsDeny((prev) => {
        const cur = prev ?? [];
        if (cur.includes(toolName)) return cur.filter((t) => t !== toolName);
        return [...cur, toolName];
      });
    }
  };

  const canSubmit =
    name.trim() !== '' &&
    (scope !== 'client' || clientId !== '') &&
    (scope !== 'workspace' || workspaceId !== '');

  const submit = () => {
    if (!canSubmit) return;
    setSubmitError(null);

    const cleanList = (arr: string[] | null): string[] | null => {
      if (!arr || arr.length === 0) return null;
      return arr;
    };

    const fail = (err: unknown) => {
      setSubmitError(t('role.saveError', { error: getApiErrorMessage(err) }));
    };

    const base = {
      name: name.trim(),
      description: description.trim(),
      system_prompt: systemPrompt.trim(),
      tools_allow: cleanList(toolsAllow),
      tools_deny: cleanList(toolsDeny),
      model_override: modelOverride.trim() || null,
      mode,
      scope_type: scope,
    };

    if (isEdit && role) {
      const req: UpdateRoleRequest = { ...base };
      updateMutation.mutate(
        { id: role.id, ...req },
        { onSuccess: (_, vars) => { void vars; } },
      );
    } else {
      const req: CreateRoleRequest = {
        ...base,
        ...(scope === 'client' ? { client_id: clientId } : {}),
        ...(scope === 'workspace' ? { workspace_id: workspaceId } : {}),
      };
      createMutation.mutate(req, {
        onSuccess: () => {},
        onError: fail,
      });
    }
  };

  const busy = createMutation.isPending || updateMutation.isPending;

  const renderToolsCheckboxGroup = (
    tools: readonly string[],
    label: string,
    target: 'allow' | 'deny',
  ) => {
    const selected = target === 'allow' ? (toolsAllow ?? []) : (toolsDeny ?? []);
    return (
      <div key={label} className="space-y-1.5">
        <p className="text-xs font-medium text-muted-foreground">{label}</p>
        <div className="flex flex-wrap gap-x-3 gap-y-1">
          {tools.map((tool) => (
            <label
              key={tool}
              className="flex items-center gap-1.5 text-xs text-foreground/80"
            >
              <input
                type="checkbox"
                checked={selected.includes(tool)}
                onChange={() => toggleTool(tool, target)}
                className="h-3.5 w-3.5 rounded border-gray-300"
              />
              {tool}
            </label>
          ))}
        </div>
      </div>
    );
  };

  return (
    <Dialog open={open} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>{isEdit ? t('role.editRole') : t('role.newRole')}</DialogTitle>
        </DialogHeader>
        <div className="space-y-4">
          {/* 名称 */}
          <div className="space-y-2">
            <Label>{t('role.name')}</Label>
            <Input
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder={t('role.namePlaceholder')}
              aria-label={t('role.name')}
              disabled={isBuiltin}
            />
            {isBuiltin && (
              <p className="text-xs text-muted-foreground">{t('role.builtinNameHint')}</p>
            )}
          </div>

          {/* 描述 */}
          <div className="space-y-2">
            <Label>{t('role.description')}</Label>
            <Input
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              placeholder={t('role.descriptionPlaceholder')}
              aria-label={t('role.description')}
            />
          </div>

          {/* 系统提示词 */}
          <div className="space-y-2">
            <Label>{t('role.systemPrompt')}</Label>
            <textarea
              value={systemPrompt}
              onChange={(e) => setSystemPrompt(e.target.value)}
              rows={6}
              placeholder={t('role.systemPromptPlaceholder')}
              aria-label={t('role.systemPrompt')}
              className="w-full resize-y rounded-md border border-input bg-background px-3 py-2 font-mono text-sm"
            />
          </div>

          {/* 工具白名单/黑名单 */}
          <div className="space-y-2">
            <Label>{t('role.tools')}</Label>
            <div className="flex gap-1">
              <Button
                variant={toolsTab === 'allow' ? 'default' : 'outline'}
                size="sm"
                onClick={() => setToolsTab('allow')}
              >
                {t('role.toolsAllow')} {toolsAllow ? `(${toolsAllow.length})` : ''}
              </Button>
              <Button
                variant={toolsTab === 'deny' ? 'default' : 'outline'}
                size="sm"
                onClick={() => setToolsTab('deny')}
              >
                {t('role.toolsDeny')} {toolsDeny ? `(${toolsDeny.length})` : ''}
              </Button>
            </div>
            <p className="text-xs text-muted-foreground">{t('role.toolsHint')}</p>
            <div className="space-y-3 rounded-md border p-3">
              {TOOLS_GROUPED.map((group) =>
                toolsTab === 'allow'
                  ? renderToolsCheckboxGroup(group.tools, group.label, 'allow')
                  : renderToolsCheckboxGroup(group.tools, group.label, 'deny'),
              )}
            </div>
          </div>

          {/* 模型覆盖 */}
          <div className="space-y-2">
            <Label>{t('role.modelOverride')}</Label>
            <select
              value={modelOverride}
              onChange={(e) => setModelOverride(e.target.value)}
              aria-label={t('role.modelOverride')}
              className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
            >
              <option value="">{t('role.modelInherit')}</option>
              {(modelOptions?.models ?? []).length > 0 && (
                <optgroup label={t('agent.model')}>
                  {modelOptions!.models.map((m) => (
                    <option key={m.id} value={m.label}>{m.label}</option>
                  ))}
                </optgroup>
              )}
              {(modelOptions?.groups ?? []).length > 0 && (
                <optgroup label={t('agent.modelGroups')}>
                  {modelOptions!.groups.map((g) => (
                    <option key={g.id} value={g.label}>{g.label}</option>
                  ))}
                </optgroup>
              )}
            </select>
          </div>

          {/* 模式 */}
          <div className="space-y-2">
            <Label>{t('role.mode')}</Label>
            <div className="flex gap-3">
              {MODE_OPTIONS.map((opt) => (
                <label key={opt.value} className="flex items-center gap-1.5 text-sm">
                  <input
                    type="radio"
                    name="role-mode"
                    value={opt.value}
                    checked={mode === opt.value}
                    onChange={() => setMode(opt.value)}
                  />
                  {t(opt.labelKey)}
                </label>
              ))}
            </div>
          </div>

          {/* 作用域 */}
          <div className="space-y-2">
            <Label>{t('role.scope')}</Label>
            <div className="flex gap-3">
              {SCOPE_OPTIONS.map((opt) => (
                <label key={opt.value} className="flex items-center gap-1.5 text-sm">
                  <input
                    type="radio"
                    name="role-scope"
                    value={opt.value}
                    checked={scope === opt.value}
                    onChange={() => changeScope(opt.value)}
                  />
                  {t(opt.labelKey)}
                </label>
              ))}
            </div>
          </div>
          {scope === 'client' && (
            <div className="space-y-2">
              <Label>{t('role.clientLabel')}</Label>
              <select
                value={clientId}
                onChange={(e) => setClientId(e.target.value)}
                aria-label={t('role.clientLabel')}
                className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
              >
                <option value="">{t('role.clientPlaceholder')}</option>
                {(clients ?? []).map((c) => (
                  <option key={c.name} value={c.name}>
                    {c.name}
                  </option>
                ))}
              </select>
            </div>
          )}
          {scope === 'workspace' && (
            <div className="space-y-2">
              <Label>{t('role.workspaceLabel')}</Label>
              <select
                value={workspaceId}
                onChange={(e) => setWorkspaceId(e.target.value)}
                aria-label={t('role.workspaceLabel')}
                className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
              >
                <option value="">{t('role.workspacePlaceholder')}</option>
                {(workspaces ?? []).map((w) => (
                  <option key={w.id} value={w.id}>
                    {w.name}
                  </option>
                ))}
              </select>
            </div>
          )}
        </div>
        {submitError && <p className="text-sm text-destructive">{submitError}</p>}
        <DialogFooter>
          <Button variant="outline" onClick={onClose} disabled={busy}>
            {t('common.cancel')}
          </Button>
          <Button onClick={submit} disabled={busy || !canSubmit}>
            {busy && <Loader2 className="mr-1 h-4 w-4 animate-spin" />}
            {t('common.save')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

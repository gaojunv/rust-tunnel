import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter } from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Button } from '@/components/ui/button';
import { Loader2 } from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import { useAgentWorkspaces, useClients, useCreateSkill, useUpdateSkill } from '@/api/hooks';
import type { AgentMemoryScope, AgentSkill } from '@/types';

interface Props {
  open: boolean;
  onClose: () => void;
  /** 传入则为编辑模式；null/undefined 为手动新建。 */
  skill?: AgentSkill | null;
  /** 新建成功回调（携带创建的技能）。 */
  onCreated?: (skill: AgentSkill) => void;
}

const parseTags = (s: string): string[] =>
  s
    .split(',')
    .map((tag) => tag.trim())
    .filter(Boolean);

export default function SkillDialog({ open, onClose, skill = null, onCreated }: Props) {
  const { t } = useTranslation();
  const createMutation = useCreateSkill();
  const updateMutation = useUpdateSkill();
  const { data: clients } = useClients();
  const { data: workspaces } = useAgentWorkspaces();

  const isEdit = !!skill;
  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [content, setContent] = useState('');
  const [tagsStr, setTagsStr] = useState('');
  const [scope, setScope] = useState<AgentMemoryScope>('workspace');
  const [clientId, setClientId] = useState('');
  const [workspaceId, setWorkspaceId] = useState('');
  const [submitError, setSubmitError] = useState<string | null>(null);

  // initRef：每个 open 周期初始化一次表单。skill 对象随列表 refetch 变化
  // 身份，重跑初始化会覆盖进行中的编辑（仿 KbDialog 防覆盖模式）。
  const initRef = useRef(false);
  useEffect(() => {
    if (!open) {
      initRef.current = false;
      return;
    }
    if (initRef.current) return;
    initRef.current = true;
    if (skill) {
      setName(skill.name ?? '');
      setDescription(skill.description ?? '');
      setContent(skill.content ?? '');
      setTagsStr(Array.isArray(skill.tags) ? skill.tags.join(', ') : '');
      setScope(skill.scope_type ?? 'workspace');
      setClientId(skill.client_id ?? '');
      setWorkspaceId(skill.workspace_id ?? '');
    } else {
      setName('');
      setDescription('');
      setContent('');
      setTagsStr('');
      setScope('workspace');
      setClientId('');
      setWorkspaceId('');
    }
    setSubmitError(null);
  }, [open, skill]);

  const changeScope = (s: AgentMemoryScope) => {
    setScope(s);
    setClientId('');
    setWorkspaceId('');
  };

  // 新建：非 global 作用域必须绑定 client/workspace（编辑模式沿用既有坐标）。
  const canSubmit =
    (name ?? '').trim() !== '' &&
    (content ?? '').trim() !== '' &&
    (scope !== 'client' || clientId !== '') &&
    (scope !== 'workspace' || workspaceId !== '');

  const submit = () => {
    if (!canSubmit) return;
    setSubmitError(null);
    const tags = parseTags(tagsStr);
    const fail = (err: unknown) => {
      setSubmitError(t('skill.saveError', { error: getApiErrorMessage(err) }));
    };
    if (isEdit && skill) {
      updateMutation.mutate(
        {
          id: skill.id,
          name: (name ?? '').trim(),
          description: (description ?? '').trim(),
          content: (content ?? '').trim(),
          scope_type: scope,
          tags,
        },
        { onSuccess: onClose, onError: fail },
      );
    } else {
      createMutation.mutate(
        {
          name: (name ?? '').trim(),
          description: (description ?? '').trim(),
          content: (content ?? '').trim(),
          scope_type: scope,
          ...(scope === 'client' ? { client_id: clientId } : {}),
          ...(scope === 'workspace' ? { workspace_id: workspaceId } : {}),
          tags,
        },
        {
          onSuccess: (s) => {
            onClose();
            onCreated?.(s);
          },
          onError: fail,
        },
      );
    }
  };

  const busy = createMutation.isPending || updateMutation.isPending;

  return (
    <Dialog open={open} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>{isEdit ? t('skill.editSkill') : t('skill.newSkill')}</DialogTitle>
        </DialogHeader>
        <div className="space-y-4">
          <div className="space-y-2">
            <Label>{t('skill.name')}</Label>
            <Input
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder={t('skill.namePlaceholder')}
              aria-label={t('skill.name')}
            />
          </div>
          <div className="space-y-2">
            <Label>{t('skill.description')}</Label>
            <Input
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              placeholder={t('skill.descriptionPlaceholder')}
              aria-label={t('skill.description')}
            />
          </div>
          <div className="space-y-2">
            <Label>{t('skill.content')}</Label>
            <textarea
              value={content}
              onChange={(e) => setContent(e.target.value)}
              rows={6}
              placeholder={t('skill.contentPlaceholder')}
              aria-label={t('skill.content')}
              className="w-full resize-y rounded-md border border-input bg-background px-3 py-2 font-mono text-sm"
            />
          </div>
          <div className="space-y-2">
            <Label>{t('skill.scopeLabel')}</Label>
            <select
              value={scope}
              onChange={(e) => changeScope(e.target.value as AgentMemoryScope)}
              aria-label={t('skill.scopeLabel')}
              className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
            >
              <option value="global">{t('skill.scope_global')}</option>
              <option value="client">{t('skill.scope_client')}</option>
              <option value="workspace">{t('skill.scope_workspace')}</option>
            </select>
          </div>
          {!isEdit && scope === 'client' && (
            <div className="space-y-2">
              <Label>{t('skill.clientLabel')}</Label>
              <select
                value={clientId}
                onChange={(e) => setClientId(e.target.value)}
                aria-label={t('skill.clientLabel')}
                className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
              >
                <option value="">{t('skill.clientPlaceholder')}</option>
                {(clients ?? []).map((c) => (
                  <option key={c.name} value={c.name}>
                    {c.name}
                  </option>
                ))}
              </select>
            </div>
          )}
          {!isEdit && scope === 'workspace' && (
            <div className="space-y-2">
              <Label>{t('skill.workspaceLabel')}</Label>
              <select
                value={workspaceId}
                onChange={(e) => setWorkspaceId(e.target.value)}
                aria-label={t('skill.workspaceLabel')}
                className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
              >
                <option value="">{t('skill.workspacePlaceholder')}</option>
                {(workspaces ?? []).map((w) => (
                  <option key={w.id} value={w.id}>
                    {w.name}
                  </option>
                ))}
              </select>
            </div>
          )}
          <div className="space-y-2">
            <Label>{t('skill.tags')}</Label>
            <Input
              value={tagsStr}
              onChange={(e) => setTagsStr(e.target.value)}
              placeholder={t('skill.tagsPlaceholder')}
              aria-label={t('skill.tags')}
            />
          </div>
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

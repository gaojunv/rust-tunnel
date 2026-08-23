import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter } from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Button } from '@/components/ui/button';
import { Loader2 } from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import { useAgentWorkspaces, useClients, useCreateWiki, useUpdateWiki } from '@/api/hooks';
import type { AgentMemoryScope, AgentWiki } from '@/types';

interface Props {
  open: boolean;
  onClose: () => void;
  /** 传入则为编辑模式；null/undefined 为手动新建。 */
  wiki?: AgentWiki | null;
  /** 新建成功回调（携带创建的容器）。 */
  onCreated?: (wiki: AgentWiki) => void;
}

export default function WikiDialog({ open, onClose, wiki = null, onCreated }: Props) {
  const { t } = useTranslation();
  const createMutation = useCreateWiki();
  const updateMutation = useUpdateWiki();
  const { data: clients } = useClients();
  const { data: workspaces } = useAgentWorkspaces();

  const isEdit = !!wiki;
  const [name, setName] = useState('');
  const [summary, setSummary] = useState('');
  const [scope, setScope] = useState<AgentMemoryScope>('workspace');
  const [clientId, setClientId] = useState('');
  const [workspaceId, setWorkspaceId] = useState('');
  const [submitError, setSubmitError] = useState<string | null>(null);

  // initRef：每个 open 周期初始化一次表单。wiki 对象随列表 refetch 变化
  // 身份，重跑初始化会覆盖进行中的编辑（仿 KbDialog 防覆盖模式）。
  const initRef = useRef(false);
  useEffect(() => {
    if (!open) {
      initRef.current = false;
      return;
    }
    if (initRef.current) return;
    initRef.current = true;
    if (wiki) {
      setName(wiki.name);
      setSummary(wiki.summary);
      setScope(wiki.scope_type);
      setClientId(wiki.client_id);
      setWorkspaceId(wiki.workspace_id);
    } else {
      setName('');
      setSummary('');
      setScope('workspace');
      setClientId('');
      setWorkspaceId('');
    }
    setSubmitError(null);
  }, [open, wiki]);

  const changeScope = (s: AgentMemoryScope) => {
    setScope(s);
    setClientId('');
    setWorkspaceId('');
  };

  // 新建：非 global 作用域必须绑定 client/workspace（编辑模式沿用既有坐标）。
  const canSubmit =
    name.trim() !== '' &&
    (scope !== 'client' || clientId !== '') &&
    (scope !== 'workspace' || workspaceId !== '');

  const submit = () => {
    if (!canSubmit) return;
    setSubmitError(null);
    const fail = (err: unknown) => {
      setSubmitError(t('wiki.saveError', { error: getApiErrorMessage(err) }));
    };
    if (isEdit && wiki) {
      updateMutation.mutate(
        { id: wiki.id, name: name.trim(), summary: summary.trim() },
        { onSuccess: onClose, onError: fail },
      );
    } else {
      createMutation.mutate(
        {
          name: name.trim(),
          summary: summary.trim(),
          scope_type: scope,
          ...(scope === 'client' ? { client_id: clientId } : {}),
          ...(scope === 'workspace' ? { workspace_id: workspaceId } : {}),
        },
        {
          onSuccess: (w) => {
            onClose();
            onCreated?.(w);
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
          <DialogTitle>{isEdit ? t('wiki.editWiki') : t('wiki.newWiki')}</DialogTitle>
        </DialogHeader>
        <div className="space-y-4">
          <div className="space-y-2">
            <Label>{t('wiki.name')}</Label>
            <Input
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder={t('wiki.namePlaceholder')}
              aria-label={t('wiki.name')}
            />
          </div>
          <div className="space-y-2">
            <Label>{t('wiki.summary')}</Label>
            <Input
              value={summary}
              onChange={(e) => setSummary(e.target.value)}
              placeholder={t('wiki.summaryPlaceholder')}
              aria-label={t('wiki.summary')}
            />
          </div>
          <div className="space-y-2">
            <Label>{t('wiki.scopeLabel')}</Label>
            <select
              value={scope}
              onChange={(e) => changeScope(e.target.value as AgentMemoryScope)}
              aria-label={t('wiki.scopeLabel')}
              className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
            >
              <option value="global">{t('wiki.scope_global')}</option>
              <option value="client">{t('wiki.scope_client')}</option>
              <option value="workspace">{t('wiki.scope_workspace')}</option>
            </select>
          </div>
          {!isEdit && scope === 'client' && (
            <div className="space-y-2">
              <Label>{t('wiki.clientLabel')}</Label>
              <select
                value={clientId}
                onChange={(e) => setClientId(e.target.value)}
                aria-label={t('wiki.clientLabel')}
                className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
              >
                <option value="">{t('wiki.clientPlaceholder')}</option>
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
              <Label>{t('wiki.workspaceLabel')}</Label>
              <select
                value={workspaceId}
                onChange={(e) => setWorkspaceId(e.target.value)}
                aria-label={t('wiki.workspaceLabel')}
                className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
              >
                <option value="">{t('wiki.workspacePlaceholder')}</option>
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

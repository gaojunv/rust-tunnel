import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter } from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Button } from '@/components/ui/button';
import { Loader2 } from 'lucide-react';
import { getApiErrorMessage } from '@/api/client';
import { useAgentWorkspaces, useClients, useCreateMemory } from '@/api/hooks';
import type { AgentMemory, AgentMemoryScope } from '@/types';

interface Props {
  open: boolean;
  onClose: () => void;
  /** 新建成功回调（携带创建的记忆）。 */
  onCreated?: (memory: AgentMemory) => void;
}

const parseTags = (s: string): string[] =>
  s
    .split(',')
    .map((tag) => tag.trim())
    .filter(Boolean);

export default function MemoryDialog({ open, onClose, onCreated }: Props) {
  const { t } = useTranslation();
  const createMutation = useCreateMemory();
  const { data: clients } = useClients();
  const { data: workspaces } = useAgentWorkspaces();

  const [content, setContent] = useState('');
  const [tagsStr, setTagsStr] = useState('');
  const [scope, setScope] = useState<AgentMemoryScope>('workspace');
  const [clientId, setClientId] = useState('');
  const [workspaceId, setWorkspaceId] = useState('');
  const [confidence, setConfidence] = useState(0.8);
  const [submitError, setSubmitError] = useState<string | null>(null);

  // initRef：每个 open 周期重置一次表单。
  const initRef = useRef(false);
  useEffect(() => {
    if (!open) {
      initRef.current = false;
      return;
    }
    if (initRef.current) return;
    initRef.current = true;
    setContent('');
    setTagsStr('');
    setScope('workspace');
    setClientId('');
    setWorkspaceId('');
    setConfidence(0.8);
    setSubmitError(null);
  }, [open]);

  const changeScope = (s: AgentMemoryScope) => {
    setScope(s);
    setClientId('');
    setWorkspaceId('');
  };

  // 新建：非 global 作用域必须绑定 client/workspace。
  const canSubmit =
    content.trim() !== '' &&
    (scope !== 'client' || clientId !== '') &&
    (scope !== 'workspace' || workspaceId !== '');

  const submit = () => {
    if (!canSubmit) return;
    setSubmitError(null);
    const tags = parseTags(tagsStr);
    createMutation.mutate(
      {
        content: content.trim(),
        scope,
        ...(scope === 'client' ? { client_id: clientId } : {}),
        ...(scope === 'workspace' ? { workspace_id: workspaceId } : {}),
        tags,
        confidence,
      },
      {
        onSuccess: (m) => {
          onClose();
          onCreated?.(m);
        },
        onError: (err) => {
          setSubmitError(t('memory.saveError', { error: getApiErrorMessage(err) }));
        },
      },
    );
  };

  const busy = createMutation.isPending;

  return (
    <Dialog open={open} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle>{t('memory.newMemory')}</DialogTitle>
        </DialogHeader>
        <div className="space-y-4">
          <div className="space-y-2">
            <Label>{t('memory.content')}</Label>
            <textarea
              value={content}
              onChange={(e) => setContent(e.target.value)}
              rows={4}
              placeholder={t('memory.contentPlaceholder')}
              aria-label={t('memory.content')}
              className="w-full resize-y rounded-md border border-input bg-background px-3 py-2 text-sm"
            />
          </div>
          <div className="space-y-2">
            <Label>{t('memory.scopeLabel')}</Label>
            <select
              value={scope}
              onChange={(e) => changeScope(e.target.value as AgentMemoryScope)}
              aria-label={t('memory.scopeLabel')}
              className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
            >
              <option value="global">{t('memory.scope_global')}</option>
              <option value="client">{t('memory.scope_client')}</option>
              <option value="workspace">{t('memory.scope_workspace')}</option>
            </select>
          </div>
          {scope === 'client' && (
            <div className="space-y-2">
              <Label>{t('memory.clientLabel')}</Label>
              <select
                value={clientId}
                onChange={(e) => setClientId(e.target.value)}
                aria-label={t('memory.clientLabel')}
                className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
              >
                <option value="">{t('memory.clientPlaceholder')}</option>
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
              <Label>{t('memory.workspaceLabel')}</Label>
              <select
                value={workspaceId}
                onChange={(e) => setWorkspaceId(e.target.value)}
                aria-label={t('memory.workspaceLabel')}
                className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
              >
                <option value="">{t('memory.workspacePlaceholder')}</option>
                {(workspaces ?? []).map((w) => (
                  <option key={w.id} value={w.id}>
                    {w.name}
                  </option>
                ))}
              </select>
            </div>
          )}
          <div className="space-y-2">
            <Label>{t('memory.tags')}</Label>
            <Input
              value={tagsStr}
              onChange={(e) => setTagsStr(e.target.value)}
              placeholder={t('memory.tagsPlaceholder')}
              aria-label={t('memory.tags')}
            />
          </div>
          <div className="space-y-2">
            <Label>
              {t('memory.confidence')}: {confidence.toFixed(2)}
            </Label>
            <input
              type="range"
              min={0}
              max={1}
              step={0.05}
              value={confidence}
              onChange={(e) => setConfidence(Number(e.target.value))}
              aria-label={t('memory.confidence')}
              className="w-full"
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

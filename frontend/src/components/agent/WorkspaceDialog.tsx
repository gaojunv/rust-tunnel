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
  updateAgentWorkspace,
} from '@/api/client';
import type { AgentWorkspace, Client } from '@/types';

interface Props {
  /** 传入则为编辑模式：仅可改 name/root_path/system_prompt/approval_mode，client/运行时不可变 */
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
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // 新建流程 create 成功后、补写 PUT 失败时记录已创建的工作区：用户点「创建」重试
  // 时不再重复 create（只重试补写），避免生成重复工作区。
  const createdRef = useRef<AgentWorkspace | null>(null);

  const canSubmit = editing
    ? name.trim() !== '' && rootPath.trim() !== ''
    : name.trim() !== '' &&
      clientId !== '' &&
      rootPath.trim() !== '' &&
      (runtimeType === 'host' || (dockerImage.trim() !== '' && dockerContainerId.trim() !== ''));

  const submit = async () => {
    if (!canSubmit || submitting) return;
    setSubmitting(true);
    setError(null);
    try {
      let w: AgentWorkspace;
      if (editing) {
        // 编辑：PUT 必须带上完整 name/root_path/system_prompt（空串即清除，缺省会被
        // 后端当作「未设置」而不是「清空」，形成缺省即清除陷阱）。
        await updateAgentWorkspace(editing.id, {
          name: name.trim(),
          root_path: rootPath.trim(),
          system_prompt: systemPrompt.trim(),
          approval_mode: approvalMode,
        });
        w = {
          ...editing,
          name: name.trim(),
          root_path: rootPath.trim(),
          system_prompt: systemPrompt.trim() || null,
          approval_mode: approvalMode,
        };
      } else {
        w = createdRef.current ?? (await createAgentWorkspace({
          name: name.trim(),
          client_id: clientId,
          runtime_type: runtimeType,
          root_path: rootPath.trim(),
          docker_image: runtimeType === 'docker' ? dockerImage.trim() : undefined,
          docker_container_id: runtimeType === 'docker' ? dockerContainerId.trim() : undefined,
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

import { useState } from 'react';
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
import { clientsApi, createAgentWorkspace, getApiErrorMessage } from '@/api/client';
import type { AgentWorkspace, Client } from '@/types';

interface Props {
  onClose: () => void;
  onCreated: (w: AgentWorkspace) => void;
}

export default function WorkspaceDialog({ onClose, onCreated }: Props) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const { data: clients, isLoading } = useQuery<Client[]>({
    queryKey: ['clients'],
    queryFn: clientsApi.list,
  });

  const [name, setName] = useState('');
  const [clientId, setClientId] = useState('');
  const [runtimeType, setRuntimeType] = useState<'host' | 'docker'>('host');
  const [rootPath, setRootPath] = useState('');
  const [dockerImage, setDockerImage] = useState('');
  const [dockerContainerId, setDockerContainerId] = useState('');
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const canSubmit =
    name.trim() !== '' &&
    clientId !== '' &&
    rootPath.trim() !== '' &&
    (runtimeType === 'host' || (dockerImage.trim() !== '' && dockerContainerId.trim() !== ''));

  const submit = async () => {
    if (!canSubmit || submitting) return;
    setSubmitting(true);
    setError(null);
    try {
      const w = await createAgentWorkspace({
        name: name.trim(),
        client_id: clientId,
        runtime_type: runtimeType,
        root_path: rootPath.trim(),
        docker_image: runtimeType === 'docker' ? dockerImage.trim() : undefined,
        docker_container_id: runtimeType === 'docker' ? dockerContainerId.trim() : undefined,
      });
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
          <DialogTitle>{t('agent.newWorkspace')}</DialogTitle>
        </DialogHeader>
        <div className="space-y-4">
          <div className="space-y-2">
            <Label>{t('agent.name')}</Label>
            <Input value={name} onChange={(e) => setName(e.target.value)} placeholder={t('agent.namePlaceholder')} />
          </div>
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
          <div className="space-y-2">
            <Label>{t('agent.rootPath')}</Label>
            <Input
              value={rootPath}
              onChange={(e) => setRootPath(e.target.value)}
              placeholder={runtimeType === 'host' ? t('agent.rootPathPlaceholderHost') : t('agent.rootPathPlaceholderDocker')}
            />
          </div>
          {runtimeType === 'docker' && (
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
            {t('agent.create')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

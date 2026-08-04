import { useState } from 'react';
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
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const canSubmit =
    name.trim() !== '' &&
    clientId !== '' &&
    rootPath.trim() !== '' &&
    (runtimeType === 'host' || dockerImage.trim() !== '');

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
          <DialogTitle>新建工作区</DialogTitle>
        </DialogHeader>
        <div className="space-y-4">
          <div className="space-y-2">
            <Label>名称</Label>
            <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="例如 my-project" />
          </div>
          <div className="space-y-2">
            <Label>客户端</Label>
            <select
              value={clientId}
              onChange={(e) => setClientId(e.target.value)}
              disabled={isLoading}
              className="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm disabled:cursor-not-allowed disabled:opacity-50"
            >
              <option value="">
                {isLoading ? '加载中…' : '选择客户端…'}
              </option>
              {(clients ?? []).map((c) => (
                <option key={c.name} value={c.name}>
                  {c.name}
                  {c.online ? '' : '（离线）'}
                </option>
              ))}
            </select>
          </div>
          <div className="space-y-2">
            <Label>运行时类型</Label>
            <div className="flex gap-4">
              <label className="flex items-center gap-2 text-sm">
                <input
                  type="radio"
                  checked={runtimeType === 'host'}
                  onChange={() => setRuntimeType('host')}
                />
                Host（宿主机执行）
              </label>
              <label className="flex items-center gap-2 text-sm">
                <input
                  type="radio"
                  checked={runtimeType === 'docker'}
                  onChange={() => setRuntimeType('docker')}
                />
                Docker
              </label>
            </div>
          </div>
          <div className="space-y-2">
            <Label>工作目录（root_path）</Label>
            <Input
              value={rootPath}
              onChange={(e) => setRootPath(e.target.value)}
              placeholder={runtimeType === 'host' ? '/home/user/project' : '/workspace'}
            />
          </div>
          {runtimeType === 'docker' && (
            <div className="space-y-2">
              <Label>Docker 镜像</Label>
              <Input
                value={dockerImage}
                onChange={(e) => setDockerImage(e.target.value)}
                placeholder="node:20"
              />
              <p className="text-xs text-muted-foreground">
                MVP 暂不支持自动创建容器：请预先启动容器，并让 root_path 指向容器内路径。
              </p>
            </div>
          )}
        </div>
        {error && <p className="text-sm text-destructive">{error}</p>}
        <DialogFooter>
          <Button variant="outline" onClick={onClose} disabled={submitting}>
            取消
          </Button>
          <Button onClick={submit} disabled={!canSubmit || submitting}>
            {submitting && <Loader2 className="mr-1 h-4 w-4 animate-spin" />}
            创建
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { Plus, Trash2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { listAgentWorkspaces, deleteAgentWorkspace } from '../../api/client';
import type { AgentWorkspace } from '../../types';

interface Props {
  workspaceId: string;
  onSelect: (id: string) => void;
  onNew: () => void;
}

/** 顶栏工作区选择：下拉 + 新建 + 删除。 */
export default function WorkspaceBar({ workspaceId, onSelect, onNew }: Props) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [confirming, setConfirming] = useState(false);
  const { data: workspaces } = useQuery<AgentWorkspace[]>({
    queryKey: ['agent-workspaces'],
    queryFn: listAgentWorkspaces,
  });

  const handleDelete = async () => {
    if (!workspaceId) return;
    await deleteAgentWorkspace(workspaceId);
    setConfirming(false);
    queryClient.invalidateQueries({ queryKey: ['agent-workspaces'] });
    onSelect(''); // 清空选中，回到引导态
  };

  return (
    <div className="flex items-center gap-2">
      <select
        value={workspaceId}
        onChange={(e) => onSelect(e.target.value)}
        className="h-9 max-w-[220px] rounded-md border border-input bg-background px-3 py-1 text-sm"
        aria-label={t('agent.selectWorkspaceAria')}
      >
        <option value="">{t('agent.selectWorkspace')}</option>
        {workspaces?.map((w) => (
          <option key={w.id} value={w.id}>
            {w.name}
          </option>
        ))}
      </select>
      <Button variant="outline" size="sm" onClick={onNew} aria-label={t('agent.newWorkspace')}>
        <Plus className="h-4 w-4" />
      </Button>
      {workspaceId &&
        (confirming ? (
          <>
            <span className="text-xs text-destructive">{t('agent.confirmDeleteWorkspace')}</span>
            <Button variant="destructive" size="sm" onClick={handleDelete}>
              {t('agent.confirm')}
            </Button>
            <Button variant="ghost" size="sm" onClick={() => setConfirming(false)}>
              {t('agent.cancel')}
            </Button>
          </>
        ) : (
          <Button
            variant="ghost"
            size="sm"
            onClick={() => setConfirming(true)}
            aria-label={t('agent.deleteWorkspace')}
          >
            <Trash2 className="h-4 w-4 text-destructive" />
          </Button>
        ))}
    </div>
  );
}

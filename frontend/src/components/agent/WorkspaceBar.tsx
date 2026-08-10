import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { Loader2, Plus, Settings, Trash2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { listAgentWorkspaces, deleteAgentWorkspace, getApiErrorMessage } from '../../api/client';
import type { AgentWorkspace } from '../../types';

/** shadcn Select 不允许空字符串 value：用哨兵值表示「未选择」，回调时映射回 ''。 */
const NONE_VALUE = '__none__';

interface Props {
  workspaceId: string;
  onSelect: (id: string) => void;
  onNew: () => void;
  /** 编辑当前工作区（齿轮入口，需已选中工作区） */
  onEdit: () => void;
}

/** 顶栏工作区选择：shadcn Select（选项含 client_id 小字）+ 编辑/新建 +
 *  Dialog 确认删除（替代原 inline 两段确认——确认态文本+双按钮挤在顶栏一行，
 *  会撑破布局；对话框也避免误触）。 */
export default function WorkspaceBar({ workspaceId, onSelect, onNew, onEdit }: Props) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [confirming, setConfirming] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { data: workspaces } = useQuery<AgentWorkspace[]>({
    queryKey: ['agent-workspaces'],
    queryFn: listAgentWorkspaces,
  });

  const current = workspaces?.find((w) => w.id === workspaceId);

  const handleDelete = async () => {
    if (!workspaceId || deleting) return;
    setDeleting(true);
    try {
      await deleteAgentWorkspace(workspaceId);
      setError(null);
      setConfirming(false);
      queryClient.invalidateQueries({ queryKey: ['agent-workspaces'] });
      onSelect(''); // 清空选中，回到引导态
    } catch (err) {
      setError(getApiErrorMessage(err));
    } finally {
      setDeleting(false);
    }
  };

  return (
    <div className="flex items-center gap-2">
      <Select
        // 未选择态走哨兵值（radix 空串 = 清除选择，语义不同）
        value={workspaceId || NONE_VALUE}
        onValueChange={(v) => onSelect(v === NONE_VALUE ? '' : v)}
      >
        <SelectTrigger
          className="h-9 w-[130px] md:w-[220px]"
          aria-label={t('agent.selectWorkspaceAria')}
        >
          <SelectValue placeholder={t('agent.selectWorkspace')} />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value={NONE_VALUE}>{t('agent.selectWorkspace')}</SelectItem>
          {workspaces?.map((w) => (
            <SelectItem key={w.id} value={w.id}>
              {/* 名称 + client_id 小字：多客户端部署时帮助区分同名工作区 */}
              <span className="flex w-full items-baseline justify-between gap-3">
                <span className="truncate">{w.name}</span>
                <span className="shrink-0 text-xs text-muted-foreground">{w.client_id}</span>
              </span>
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      <Button
        variant="ghost"
        size="sm"
        onClick={onEdit}
        disabled={!workspaceId}
        className="hidden md:inline-flex"
        aria-label={t('agent.editWorkspace')}
      >
        <Settings className="h-4 w-4" />
      </Button>
      <Button variant="outline" size="sm" onClick={onNew} aria-label={t('agent.newWorkspace')}>
        <Plus className="h-4 w-4" />
      </Button>
      {workspaceId && (
        <Button
          variant="ghost"
          size="sm"
          onClick={() => {
            setError(null);
            setConfirming(true);
          }}
          className="hidden md:inline-flex"
          aria-label={t('agent.deleteWorkspace')}
        >
          <Trash2 className="h-4 w-4 text-destructive" />
        </Button>
      )}

      <Dialog open={confirming} onOpenChange={setConfirming}>
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>{t('agent.deleteWorkspace')}</DialogTitle>
            <DialogDescription>
              {current?.name ? `${current.name} — ` : ''}
              {t('agent.confirmDeleteWorkspace')}
            </DialogDescription>
          </DialogHeader>
          {error && (
            <p className="text-sm text-destructive" role="alert" aria-live="polite">
              {error}
            </p>
          )}
          <DialogFooter>
            <Button variant="outline" onClick={() => setConfirming(false)} disabled={deleting}>
              {t('agent.cancel')}
            </Button>
            <Button variant="destructive" onClick={handleDelete} disabled={deleting}>
              {deleting && <Loader2 className="mr-1 h-4 w-4 animate-spin" />}
              {t('agent.confirm')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

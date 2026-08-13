import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { Check, ChevronDown, FolderOpen, Loader2, Plus, Settings, Trash2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
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

interface Props {
  workspaceId: string;
  onSelect: (id: string) => void;
  onNew: () => void;
  /** 编辑当前工作区（齿轮入口，需已选中工作区） */
  onEdit: () => void;
}

/** 顶栏工作区选择：VS Code 式图标下拉。操作项全部收进下拉——sticky 新建工作区、
 *  工作区列表（含 client_id 小字）、编辑/删除操作项（分隔线以下）+ Dialog 确认删除
 *  （替代原 inline 两段确认——确认态文本+双按钮挤在顶栏一行，会撑破布局；对话框也
 *  避免误触）。顶栏仅保留 Sparkles logo + 触发器图标。 */
export default function WorkspaceBar({ workspaceId, onSelect, onNew, onEdit }: Props) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [menuOpen, setMenuOpen] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { data: workspaces } = useQuery<AgentWorkspace[]>({
    queryKey: ['agent-workspaces'],
    queryFn: listAgentWorkspaces,
  });

  const current = workspaces?.find((w) => w.id === workspaceId);

  const openDeleteConfirm = () => {
    // 先关下拉再弹 Dialog：Dialog 覆盖在打开的菜单上会造成焦点/层级混乱
    setMenuOpen(false);
    setError(null);
    setConfirming(true);
  };

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
    <div className="flex shrink-0 items-center gap-2">
      <DropdownMenu open={menuOpen} onOpenChange={setMenuOpen}>
        <DropdownMenuTrigger asChild>
          <Button
            variant="ghost"
            size="sm"
            aria-label={t('agent.selectWorkspaceAria')}
            title={current ? current.name : t('agent.selectWorkspace')}
          >
            <FolderOpen className="h-4 w-4" />
            {/* sr-only：保留可访问性 + 供测试断言触发器文本 */}
            <span className="sr-only">
              {current ? current.name : t('agent.selectWorkspace')}
            </span>
            <ChevronDown className="h-3.5 w-3.5 opacity-50" />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start" className="w-[calc(100vw-2rem)] max-w-80 p-0 md:w-72">
          {/* sticky 新建工作区：列表长时滚动仍保持可见；onSelect 触发后 Radix 自动收起菜单 */}
          <div className="sticky top-0 z-10 border-b border-border/60 bg-popover">
            <DropdownMenuItem className="cursor-pointer" onSelect={() => onNew()}>
              <Plus className="h-4 w-4" />
              {t('agent.newWorkspace')}
            </DropdownMenuItem>
          </div>
          <div className="p-1">
            {(workspaces ?? []).map((w) => (
              <DropdownMenuItem
                key={w.id}
                className="cursor-pointer"
                onSelect={() => onSelect(w.id)}
              >
                {/* 固定宽度 Check 列：非选中项仅占位透明，保证各行左对齐 */}
                <span className="flex h-4 w-4 shrink-0 items-center justify-center">
                  <Check className={w.id === workspaceId ? 'h-4 w-4' : 'h-4 w-4 opacity-0'} />
                </span>
                <div className="min-w-0 flex-1">
                  {/* 名称 + client_id 小字：多客户端部署时帮助区分同名工作区 */}
                  <div className="truncate text-sm">{w.name}</div>
                  <div className="truncate text-xs text-muted-foreground">{w.client_id}</div>
                </div>
              </DropdownMenuItem>
            ))}
            {(workspaces ?? []).length === 0 && (
              <p className="px-2 py-2 text-xs text-muted-foreground">{t('agent.selectWorkspace')}</p>
            )}
          </div>
          <DropdownMenuSeparator />
          {/* 操作区：编辑/删除收进下拉；未选中工作区时禁用 */}
          <div className="p-1">
            <DropdownMenuItem
              className="cursor-pointer"
              disabled={!workspaceId}
              onSelect={() => onEdit()}
              aria-label={t('agent.editWorkspace')}
            >
              <Settings className="h-4 w-4" />
              {t('agent.editWorkspace')}
            </DropdownMenuItem>
            <DropdownMenuItem
              className="cursor-pointer"
              disabled={!workspaceId}
              onSelect={openDeleteConfirm}
              aria-label={t('agent.deleteWorkspace')}
            >
              <Trash2 className="h-4 w-4 text-destructive" />
              {t('agent.deleteWorkspace')}
            </DropdownMenuItem>
          </div>
        </DropdownMenuContent>
      </DropdownMenu>

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

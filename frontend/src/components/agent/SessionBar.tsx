import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { ChevronDown, MessageSquare, Pencil, Plus, Trash2, Check, X } from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import {
  listAgentSessions,
  deleteAgentSession,
  updateAgentSessionTitle,
} from '../../api/client';
import type { AgentSession } from '../../types';

interface Props {
  workspaceId: string;
  sessionId: string;
  onSelect: (id: string) => void;
  /** 删除当前会话后回引导态（AgentPage 据此禁止自动重选）。 */
  onDeletedCurrent: () => void;
  onNew: () => void;
}

/** 顶栏会话选择：下拉（项内改名/删除）+ 新建。 */
export default function SessionBar({ workspaceId, sessionId, onSelect, onDeletedCurrent, onNew }: Props) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editTitle, setEditTitle] = useState('');
  const { data: sessions } = useQuery<AgentSession[]>({
    queryKey: ['agent-sessions', workspaceId],
    queryFn: () => listAgentSessions(workspaceId),
    enabled: !!workspaceId,
  });

  const current = sessions?.find((s) => s.id === sessionId);
  const refresh = () => queryClient.invalidateQueries({ queryKey: ['agent-sessions', workspaceId] });

  const handleDelete = async (id: string) => {
    await deleteAgentSession(id);
    refresh();
    if (id === sessionId) onDeletedCurrent(); // 删的是当前会话 → 回引导态（AgentPage 禁止自动重选）
  };

  const handleRename = async (id: string) => {
    const title = editTitle.trim();
    if (title) await updateAgentSessionTitle(id, title);
    setEditingId(null);
    refresh();
  };

  return (
    <div className="flex items-center gap-2">
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button variant="outline" size="sm" aria-label={t('agent.selectSessionAria')}>
            <MessageSquare className="mr-1 h-4 w-4" />
            <span className="max-w-[160px] truncate">
              {current ? current.title || t('agent.unnamedSession') : t('agent.selectSession')}
            </span>
            <ChevronDown className="ml-1 h-3.5 w-3.5 opacity-50" />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start" className="w-64">
          {(sessions ?? []).map((s) => (
            <div key={s.id} className="flex items-center gap-1 px-1">
              {editingId === s.id ? (
                <>
                  <input
                    autoFocus
                    value={editTitle}
                    onChange={(e) => setEditTitle(e.target.value)}
                    placeholder={t('agent.sessionNamePlaceholder')}
                    className="h-7 flex-1 rounded border border-input bg-background px-2 text-sm"
                    onKeyDown={(e) => {
                      if (e.key === 'Enter') handleRename(s.id);
                      if (e.key === 'Escape') setEditingId(null);
                    }}
                  />
                  <Button variant="ghost" size="sm" className="h-7 w-7 p-0" onClick={() => handleRename(s.id)}>
                    <Check className="h-3.5 w-3.5" />
                  </Button>
                  <Button variant="ghost" size="sm" className="h-7 w-7 p-0" onClick={() => setEditingId(null)}>
                    <X className="h-3.5 w-3.5" />
                  </Button>
                </>
              ) : (
                <>
                  <DropdownMenuItem className="flex-1 cursor-pointer" onSelect={() => onSelect(s.id)}>
                    <span className="truncate">{s.title || t('agent.unnamedSession')}</span>
                  </DropdownMenuItem>
                  <Button
                    variant="ghost"
                    size="sm"
                    className="h-7 w-7 p-0"
                    aria-label={t('agent.renameSession')}
                    onClick={(e) => {
                      e.preventDefault();
                      setEditingId(s.id);
                      setEditTitle(s.title ?? '');
                    }}
                  >
                    <Pencil className="h-3.5 w-3.5" />
                  </Button>
                  <Button
                    variant="ghost"
                    size="sm"
                    className="h-7 w-7 p-0"
                    aria-label={t('agent.deleteSession')}
                    onClick={(e) => {
                      e.preventDefault();
                      if (window.confirm(t('agent.confirmDeleteSession'))) void handleDelete(s.id);
                    }}
                  >
                    <Trash2 className="h-3.5 w-3.5 text-destructive" />
                  </Button>
                </>
              )}
            </div>
          ))}
          {(sessions ?? []).length === 0 && (
            <p className="px-2 py-2 text-xs text-muted-foreground">{t('agent.noSessions')}</p>
          )}
        </DropdownMenuContent>
      </DropdownMenu>
      <Button variant="outline" size="sm" onClick={onNew} aria-label={t('agent.newSession')}>
        <Plus className="h-4 w-4" />
      </Button>
    </div>
  );
}

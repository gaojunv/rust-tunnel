import { useTranslation } from 'react-i18next';
import { useQuery } from '@tanstack/react-query';
import { Plus, X } from 'lucide-react';
import { listAgentSessions } from '../../api/client';
import { MAX_TABS } from './tabsStore';
import { cn } from '@/lib/utils';

interface Props {
  workspaceId: string;
  /** 已打开的会话标签 id（有序） */
  open: string[];
  /** 当前激活标签 id */
  active: string;
  onSelect: (id: string) => void;
  onClose: (id: string) => void;
  onNew: () => void;
}

/** 浏览器标签页式多会话栏：横向滚动，激活态高亮，× 关闭（会话数据保留），尾端 + 新建。 */
export default function SessionTabBar({ workspaceId, open, active, onSelect, onClose, onNew }: Props) {
  const { t } = useTranslation();
  // 与 SessionBar 共享 queryKey：标题（session_title/done 后 invalidate）自动回显
  const { data: sessions } = useQuery({
    queryKey: ['agent-sessions', workspaceId],
    queryFn: () => listAgentSessions(workspaceId),
    enabled: !!workspaceId,
  });
  const byId = new Map((sessions ?? []).map((s) => [s.id, s]));
  const limitReached = open.length >= MAX_TABS;

  return (
    <div
      role="tablist"
      aria-label={t('agent.openTabs')}
      className="flex min-w-0 flex-1 items-center gap-0.5 overflow-x-auto"
    >
      {open.map((id) => {
        const s = byId.get(id);
        const title = s?.title || t('agent.unnamedSession');
        const isActive = id === active;
        return (
          <div
            key={id}
            role="tab"
            aria-selected={isActive}
            aria-label={title}
            tabIndex={0}
            onClick={() => onSelect(id)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                onSelect(id);
              }
            }}
            className={cn(
              'group flex max-w-[8rem] shrink-0 cursor-pointer items-center gap-1 rounded-md px-2 py-1.5 text-sm transition-colors md:max-w-[12rem]',
              isActive
                ? 'bg-primary/10 font-medium text-primary'
                : 'text-muted-foreground hover:bg-muted/60 hover:text-foreground',
            )}
          >
            <span className="min-w-0 flex-1 truncate">{title}</span>
            <button
              type="button"
              aria-label={t('agent.closeTab')}
              onClick={(e) => {
                e.stopPropagation();
                onClose(id);
              }}
              className={cn(
                'shrink-0 rounded p-0.5 hover:bg-muted hover:text-foreground',
                isActive ? 'opacity-100' : 'opacity-0 group-hover:opacity-100',
              )}
            >
              <X className="h-3.5 w-3.5" />
            </button>
          </div>
        );
      })}
      <button
        type="button"
        aria-label={t('agent.newTab')}
        title={limitReached ? t('agent.tabLimitReached', { count: MAX_TABS }) : undefined}
        disabled={limitReached}
        onClick={onNew}
        className="shrink-0 rounded p-1.5 text-muted-foreground hover:bg-muted hover:text-foreground disabled:cursor-not-allowed disabled:opacity-40"
      >
        <Plus className="h-4 w-4" />
      </button>
    </div>
  );
}

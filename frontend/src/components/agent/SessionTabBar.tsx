import { memo, useMemo } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery } from '@tanstack/react-query';
import { Plus, X } from 'lucide-react';
import { listAgentSessions } from '../../api/client';
import { MAX_TABS } from './tabsStore';
import { cn } from '@/lib/utils';
import { useMediaQuery } from '@/hooks/useMediaQuery';
import SessionBar from './SessionBar';

interface Props {
  workspaceId: string;
  /** 已打开的会话标签 id（有序） */
  open: string[];
  /** 当前激活标签 id */
  active: string;
  onSelect: (id: string) => void;
  onClose: (id: string) => void;
  onNew: () => void;
  /** 删除任意会话后回调（透传给移动端 SessionBar，关闭对应标签页）。 */
  onSessionDeleted?: (id: string) => void;
}

/** 浏览器标签页式多会话栏：横向滚动，激活态高亮，× 关闭（会话数据保留），尾端 + 新建。
 *  移动端（<768px）太挤，不铺开多标签——只显示当前会话标题，点击标题打开 SessionBar
 *  同一会话下拉（切换/新建/改名/删除都在里面），替代独立 session 图标按钮与 + 号。 */
function SessionTabBar({ workspaceId, open, active, onSelect, onClose, onNew, onSessionDeleted }: Props) {
  const { t } = useTranslation();
  // 与 SessionBar 共享 queryKey：标题（session_title/done 后 invalidate）自动回显
  const { data: sessions } = useQuery({
    queryKey: ['agent-sessions', workspaceId],
    queryFn: () => listAgentSessions(workspaceId),
    enabled: !!workspaceId,
  });
  // jsdom/SSR 无 matchMedia 时 useMediaQuery 返回 false → 走移动端单标题分支
  const isDesktop = useMediaQuery('(min-width: 768px)');
  const byId = useMemo(() => new Map((sessions ?? []).map((s) => [s.id, s])), [sessions]);
  const limitReached = open.length >= MAX_TABS;

  const newButton = (
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
  );

  // 移动端：只显示当前会话标题，点击标题打开 SessionBar 同一会话下拉（切换/新建/
  // 改名/删除入口都在里面）。不铺开多标签、不渲染独立 session 图标按钮与 + 号。
  if (!isDesktop) {
    const current = byId.get(active);
    const title = current?.title || t('agent.unnamedSession');
    return (
      <div className="flex min-w-0 flex-1 items-center">
        <SessionBar
          workspaceId={workspaceId}
          sessionId={active}
          onSelect={onSelect}
          onSessionDeleted={onSessionDeleted ?? (() => {})}
          onNew={onNew}
          triggerContent={
            <span className="min-w-0 truncate text-sm font-medium">{title}</span>
          }
          triggerClassName="h-auto min-w-0 max-w-full justify-start gap-0 px-2 py-1.5 text-foreground hover:bg-muted/60"
        />
      </div>
    );
  }

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
      {newButton}
    </div>
  );
}

export default memo(SessionTabBar);

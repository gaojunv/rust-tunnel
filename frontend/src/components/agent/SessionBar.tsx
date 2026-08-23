import { memo, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { Download, MessageSquare, Pencil, Plus, Trash2, Check, X } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useImeGuard } from '@/hooks/useImeGuard';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
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
import {
  listAgentSessions,
  deleteAgentSession,
  exportAgentSession,
  updateAgentSessionTitle,
  getApiErrorMessage,
} from '../../api/client';
import { formatRelativeTime, type TranslateFn } from './formatRelativeTime';
import type { AgentSession } from '../../types';

interface Props {
  workspaceId: string;
  sessionId: string;
  onSelect: (id: string) => void;
  /** 删除任意会话后回调（AgentPage 据此关闭对应标签页）。 */
  onSessionDeleted: (id: string) => void;
  onNew: () => void;
  /**
   * 自定义触发器内容：缺省渲染「消息图标」按钮（桌面顶栏）；移动端传入会话标题，
   * 让标题本身成为打开下拉切换会话的入口（替代独立 session 图标按钮，省横向空间）。
   */
  triggerContent?: ReactNode;
  /** 自定义触发器的额外 className（移动端标题需要 truncate/flex-1 等布局类） */
  triggerClassName?: string;
}

/** 顶栏会话选择：图标下拉（sticky 新建会话 + 列表项内改名/删除 + 信息增强）。 */
function SessionBar({ workspaceId, sessionId, onSelect, onSessionDeleted, onNew, triggerContent, triggerClassName }: Props) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [editingId, setEditingId] = useState<string | null>(null);
  const [editTitle, setEditTitle] = useState('');
  // IME 组词守卫：中文改名时回车是确认候选，不应立即提交重命名
  const ime = useImeGuard();
  const [error, setError] = useState<string | null>(null);
  // 受控打开态：删除时需先关闭下拉再弹确认 Dialog，避免两个浮层叠放
  const [menuOpen, setMenuOpen] = useState(false);
  const [deletingSession, setDeletingSession] = useState<AgentSession | null>(null);
  const { data: sessions } = useQuery<AgentSession[]>({
    queryKey: ['agent-sessions', workspaceId],
    queryFn: () => listAgentSessions(workspaceId),
    enabled: !!workspaceId,
  });

  const current = sessions?.find((s) => s.id === sessionId);
  const refresh = () => queryClient.invalidateQueries({ queryKey: ['agent-sessions', workspaceId] });
  // i18next 的 t 键是字面量联合（见 i18n/i18next.d.ts），与工具函数的宽松签名互不兼容，此处收窄一次
  const translate = t as unknown as TranslateFn;

  const handleDelete = async (session: AgentSession) => {
    setDeletingSession(null);
    try {
      await deleteAgentSession(session.id);
      setError(null);
      refresh();
      onSessionDeleted(session.id); // 任意会话被删 → 关掉对应标签页（AgentPage 据此关闭 tab）
    } catch (err) {
      setError(getApiErrorMessage(err));
    }
  };

  /** 导出会话 Markdown：fetch blob → 临时 a[download] 触发浏览器下载。 */
  const handleExport = async (session: AgentSession) => {
    try {
      const blob = await exportAgentSession(session.id);
      setError(null);
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      const safeTitle = (session.title ?? '').trim().replace(/[\\/:*?"<>|\s]+/g, '_');
      a.href = url;
      a.download = safeTitle ? `${safeTitle}.md` : `agent-session-${session.id.slice(0, 8)}.md`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (err) {
      setError(getApiErrorMessage(err));
    }
  };

  const handleRename = async (id: string) => {
    const title = editTitle.trim();
    if (!title) {
      setEditingId(null);
      return;
    }
    try {
      await updateAgentSessionTitle(id, title);
      setError(null);
      setEditingId(null);
      refresh();
    } catch (err) {
      setError(getApiErrorMessage(err));
    }
  };

  // 根容器：缺省图标触发器 shrink-0 即可；自定义触发器（移动端会话标题）必须
  // min-w-0 可收缩——否则 shrink-0 让整个触发器按标题 max-content 宽度撑开，顶穿
  // flex-1 容器延到右缘伸进 fixed 悬浮的 MobileMenuFab 区域，内部标题 truncate
  // 也因父级不受约束而失效（flex truncate 被 shrink-0 破坏陷阱）。
  return (
    <div className={triggerContent !== undefined
      ? 'flex min-w-0 flex-1 items-center gap-2'
      : 'flex shrink-0 items-center gap-2'}>
      {error && (
        <span className="text-xs text-destructive" role="alert" aria-live="polite">
          {error}
        </span>
      )}
      <DropdownMenu open={menuOpen} onOpenChange={setMenuOpen}>
        <DropdownMenuTrigger asChild>
          {triggerContent !== undefined ? (
            // 自定义触发器（移动端：会话标题本身即切换入口）
            <Button
              variant="ghost"
              size="sm"
              aria-label={t('agent.selectSessionAria')}
              title={current ? current.title || t('agent.unnamedSession') : t('agent.selectSession')}
              className={triggerClassName}
            >
              {triggerContent}
            </Button>
          ) : (
            <Button
              variant="ghost"
              size="sm"
              aria-label={t('agent.selectSessionAria')}
              title={current ? current.title || t('agent.unnamedSession') : t('agent.selectSession')}
            >
              <MessageSquare className="h-4 w-4" />
              {/* sr-only：保留可访问性 + 供测试断言触发器文本 */}
              <span className="sr-only">
                {current ? current.title || t('agent.unnamedSession') : t('agent.selectSession')}
              </span>
            </Button>
          )}
        </DropdownMenuTrigger>
        <DropdownMenuContent align="start" className="w-[calc(100vw-2rem)] max-w-80 p-0 md:w-72">
          {/* sticky 新建会话：列表长时滚动仍保持可见；onSelect 触发后 Radix 自动收起菜单 */}
          <div className="sticky top-0 z-10 border-b border-border/60 bg-popover">
            <DropdownMenuItem className="cursor-pointer" onSelect={() => onNew()}>
              <Plus className="h-4 w-4" />
              {t('agent.newSession')}
            </DropdownMenuItem>
          </div>
          <div className="p-1">
            {(sessions ?? []).map((s) => {
              // 相对时间按 updated_at 计算（最近活跃）；模型截断显示在同一行
              const relativeTime = formatRelativeTime(new Date(s.updated_at).getTime(), Date.now(), translate);
              const meta = s.model ? `${relativeTime} · ${s.model}` : relativeTime;
              return (
                <div key={s.id} className="flex items-center gap-1 px-1 py-0.5">
                  {editingId === s.id ? (
                    <>
                      <input
                        autoFocus
                        value={editTitle}
                        onChange={(e) => setEditTitle(e.target.value)}
                        placeholder={t('agent.sessionNamePlaceholder')}
                        className="h-7 flex-1 rounded border border-input bg-background px-2 text-sm"
                        {...ime.bind}
                        onKeyDown={(e) => {
                          if (ime.isComposing(e)) return;
                          if (e.key === 'Enter') handleRename(s.id);
                          if (e.key === 'Escape') setEditingId(null);
                        }}
                      />
                      <Button
                        variant="ghost"
                        size="sm"
                        className="h-7 w-7 p-0"
                        aria-label={t('common.save')}
                        onClick={() => handleRename(s.id)}
                      >
                        <Check className="h-3.5 w-3.5" />
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        className="h-7 w-7 p-0"
                        aria-label={t('common.cancel')}
                        onClick={() => setEditingId(null)}
                      >
                        <X className="h-3.5 w-3.5" />
                      </Button>
                    </>
                  ) : (
                    <>
                      <DropdownMenuItem
                        className="min-w-0 flex-1 cursor-pointer py-1.5"
                        onSelect={() => onSelect(s.id)}
                      >
                        {/* 固定宽度 Check 列：非选中项仅占位透明，保证各行左对齐 */}
                        <span className="flex h-4 w-4 shrink-0 items-center justify-center">
                          <Check className={s.id === sessionId ? 'h-4 w-4' : 'h-4 w-4 opacity-0'} />
                        </span>
                        <div className="min-w-0 flex-1">
                          <div className="truncate text-sm">{s.title || t('agent.unnamedSession')}</div>
                          <div className="truncate text-xs text-muted-foreground">{meta}</div>
                        </div>
                      </DropdownMenuItem>
                      <Button
                        variant="ghost"
                        size="sm"
                        className="h-7 w-7 p-0"
                        aria-label={t('agent.exportSession')}
                        title={t('agent.exportSession')}
                        onClick={(e) => {
                          e.preventDefault();
                          void handleExport(s);
                        }}
                      >
                        <Download className="h-3.5 w-3.5" />
                      </Button>
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
                          // 先关下拉再弹 Dialog：Dialog 覆盖在打开的菜单上会造成焦点/层级混乱
                          setMenuOpen(false);
                          setDeletingSession(s);
                        }}
                      >
                        <Trash2 className="h-3.5 w-3.5 text-destructive" />
                      </Button>
                    </>
                  )}
                </div>
              );
            })}
            {(sessions ?? []).length === 0 && (
              <p className="px-2 py-2 text-xs text-muted-foreground">{t('agent.noSessions')}</p>
            )}
          </div>
        </DropdownMenuContent>
      </DropdownMenu>

      {/* 删除确认：替代 window.confirm，复用项目 Dialog 组件（错误走上方 error 状态） */}
      <Dialog open={!!deletingSession} onOpenChange={(open) => {
        if (!open) setDeletingSession(null);
      }}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{t('agent.confirmDeleteSessionTitle')}</DialogTitle>
            <DialogDescription>
              {deletingSession
                ? t('agent.confirmDeleteSessionDesc', {
                    title: deletingSession.title || t('agent.unnamedSession'),
                  })
                : ''}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setDeletingSession(null)}>
              {t('common.cancel')}
            </Button>
            <Button
              variant="destructive"
              onClick={() => {
                if (deletingSession) void handleDelete(deletingSession);
              }}
            >
              {t('common.delete')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

export default memo(SessionBar);

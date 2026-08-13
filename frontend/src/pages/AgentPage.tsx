import { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { Sparkles } from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  listAgentWorkspaces,
  listAgentSessions,
  createAgentSession,
  getAgentDefaultModel,
} from '../api/client';
import { listAgentSelectableModels } from '../api/agentModels';
import type { AgentWorkspace, AgentSession } from '../types';
import ChatStream from '../components/agent/ChatStream';
import WorkspaceBar from '../components/agent/WorkspaceBar';
import SessionBar from '../components/agent/SessionBar';
import SessionTabBar from '../components/agent/SessionTabBar';
import ActivityBar from '../components/agent/ActivityBar';
import WorkspaceDialog from '../components/agent/WorkspaceDialog';
import {
  loadTabs,
  saveTabs,
  migrateLegacy,
  reconcile,
  openOrActivate,
  closeTab,
  type TabState,
} from '../components/agent/tabsStore';
import { useMediaQuery } from '../hooks/useMediaQuery';

export default function AgentPage() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  // 桌面/移动端分支：ActivityBar 在 <768px 下切到底部图标栏 + Sheet 面板
  const isDesktop = useMediaQuery('(min-width: 768px)');
  const [workspaceId, setWorkspaceId] = useState(
    () => localStorage.getItem('agent.lastWorkspaceId') ?? '',
  );
  // 多会话标签页：按 workspace 分桶的 tab 状态（open 有序 + active）；无持久化 = 空态
  const [tabsByWs, setTabsByWs] = useState<Record<string, TabState>>({});
  // 每个 workspace 是否已完成首次初始化（StrictMode 双调 / sessions refetch 重入守卫）
  const initedWsRef = useRef<Record<string, boolean>>({});
  // 会话模型按 tab 记忆：乐观更新 + 失败回滚（handleModelChange）写入，派生 modelFor 读取
  const [modelOverrides, setModelOverrides] = useState<Record<string, string>>({});
  const [showWorkspaceDialog, setShowWorkspaceDialog] = useState(false);
  // 编辑模式：传入当前工作区则 WorkspaceDialog 走 PUT（client/运行时不可改）
  const [editingWorkspace, setEditingWorkspace] = useState<AgentWorkspace | null>(null);

  const { data: workspaces } = useQuery<AgentWorkspace[]>({
    queryKey: ['agent-workspaces'],
    queryFn: listAgentWorkspaces,
  });

  const { data: sessions } = useQuery<AgentSession[]>({
    queryKey: ['agent-sessions', workspaceId],
    queryFn: () => listAgentSessions(workspaceId),
    enabled: !!workspaceId,
  });

  // 全局默认模型（会话无模型时回显）
  const { data: defaultModel } = useQuery({
    queryKey: ['agent-default-model'],
    queryFn: getAgentDefaultModel,
    staleTime: 60_000,
  });

  // 可用模型（会话与全局默认均未设置时回退第一个可用模型，与后端行为一致）
  const { data: selectableModels } = useQuery({
    queryKey: ['agent-selectable-models'],
    queryFn: listAgentSelectableModels,
    staleTime: 60_000,
  });

  // 当前 workspace 的 tabs（未初始化/无持久化 = 空态）。
  // useMemo 保证引用稳定，避免持久化 effect 依赖（tabs）每次渲染都变。
  const tabs = useMemo<TabState>(
    () => tabsByWs[workspaceId] ?? { open: [], active: '' },
    [tabsByWs, workspaceId],
  );

  const setTabs = (updater: (cur: TabState) => TabState) => {
    setTabsByWs((prev) => {
      const cur = prev[workspaceId] ?? { open: [], active: '' };
      return { ...prev, [workspaceId]: updater(cur) };
    });
  };

  // 只有一个 workspace 时自动选中
  useEffect(() => {
    if (!workspaceId && workspaces?.length === 1) {
      setWorkspaceId(workspaces[0].id);
    }
  }, [workspaces, workspaceId]);

  // 每个 workspace 的 tab 状态初始化 / 与 sessions 列表对齐：
  // 1. 首次：loadTabs（含空态，空态不播种）> migrateLegacy > 播种最近会话。
  // 2. 非首次：reconcile 过滤已删除的会话（主动删/他处删），有变化才更新。
  useEffect(() => {
    if (!workspaceId || !sessions) return;
    const ids = sessions.map((s) => s.id);

    if (initedWsRef.current[workspaceId]) {
      setTabsByWs((prev) => {
        const cur = prev[workspaceId] ?? { open: [], active: '' };
        const next = reconcile(cur, ids);
        if (next.open.length === cur.open.length && next.active === cur.active) return prev;
        return { ...prev, [workspaceId]: next };
      });
      return;
    }

    let state: TabState | null = loadTabs(workspaceId);
    let source: 'persisted' | 'migrated' | null = null;
    if (state !== null) {
      source = 'persisted';
    } else {
      state = migrateLegacy(workspaceId);
      if (state !== null) source = 'migrated';
    }
    if (state !== null) {
      const reconciled = reconcile(state, ids);
      // 迁移来的单标签若被过滤空（会话已删）且还有其它会话 → fall through 播种；
      // 持久化的空态（用户主动全关）尊重之，不播种。
      if (!(source === 'migrated' && reconciled.open.length === 0 && sessions.length > 0)) {
        setTabsByWs((prev) => ({ ...prev, [workspaceId]: reconciled }));
        initedWsRef.current[workspaceId] = true;
        return;
      }
    }
    // 播种：sessions 按 created_at DESC，取最近一条为单标签；无会话则空态
    setTabsByWs((prev) => ({
      ...prev,
      [workspaceId]:
        sessions.length > 0
          ? { open: [sessions[0].id], active: sessions[0].id }
          : { open: [], active: '' },
    }));
    initedWsRef.current[workspaceId] = true;
  }, [workspaceId, sessions]);

  // 刷新恢复：写入最近工作区（工作区记忆）+ 各工作区的 tab 状态（仅初始化后）
  useEffect(() => {
    if (workspaceId) localStorage.setItem('agent.lastWorkspaceId', workspaceId);
    if (workspaceId && initedWsRef.current[workspaceId]) {
      saveTabs(workspaceId, tabs);
    }
  }, [workspaceId, tabs]);

  // 会话模型派生：tab 局部覆盖优先；否则按「会话模型 → 全局默认 → 第一个可用模型」
  // 的 falsy 链回退（?? 与 || 混合处加括号，避免语法错误/歧义）。
  const modelFor = (sid: string) =>
    modelOverrides[sid] ??
    (sessions?.find((s) => s.id === sid)?.model?.trim() ||
      defaultModel?.trim() ||
      selectableModels?.models[0]?.id ||
      selectableModels?.groups[0]?.id ||
      '');

  const handleNewSession = async () => {
    if (!workspaceId) return;
    const s = await createAgentSession(workspaceId, undefined, modelFor(tabs.active) || undefined);
    // 同步写入共享缓存：让列表/标签栏立即可见新会话（无需等 invalidate）
    queryClient.setQueryData<AgentSession[]>(['agent-sessions', workspaceId], (old) => [
      s,
      ...(old ?? []),
    ]);
    setTabs((cur) => openOrActivate(cur, s.id));
  };

  const handleSelectWorkspace = (id: string) => {
    setWorkspaceId(id);
  };

  // 点击标签 / SessionBar 选择会话 → 打开或激活对应 tab
  const handleSelectSession = (id: string) => {
    setTabs((cur) => openOrActivate(cur, id));
  };

  // 关闭标签：仅关标签，会话数据保留（SessionBar 下拉仍可重新打开）
  const handleCloseTab = (id: string) => {
    setTabs((cur) => closeTab(cur, id));
  };

  // 删除会话：任意会话被删都关掉对应标签（若已打开）
  const handleSessionDeleted = (id: string) => {
    setTabs((cur) => closeTab(cur, id));
  };

  // 齿轮入口：打开编辑模式的 WorkspaceDialog（预填当前工作区，client/运行时不可改）
  const openEditWorkspace = () => {
    const w = workspaces?.find((x) => x.id === workspaceId) ?? null;
    setEditingWorkspace(w);
    setShowWorkspaceDialog(true);
  };

  // 高度 = 100dvh - Header(h-14 + 1px border = 3.5rem+1px) - 内容区上下 padding(mobile py-3=1.5rem / md py-6=3rem)。
  // -2px 留余量：3856cfc 调小高度时少减了 1px，导致外层 ScrollArea 出现整页滚动条。
  return (
    <div className="flex h-[calc(100dvh-5rem-2px)] min-h-[320px] flex-col overflow-hidden rounded-xl border border-border/70 bg-card md:h-[calc(100dvh-6.5rem-2px)] md:min-h-[480px]">
      {/* 顶栏：logo + WorkspaceBar + SessionBar */}
      <div className="flex flex-wrap items-center gap-2 border-b border-border/60 p-1.5 md:flex-nowrap md:p-2">
        <Sparkles className="h-4 w-4 shrink-0 text-primary" />
        <WorkspaceBar
          workspaceId={workspaceId}
          onSelect={handleSelectWorkspace}
          onNew={() => setShowWorkspaceDialog(true)}
          onEdit={openEditWorkspace}
        />
        {workspaceId && (
          <SessionBar
            workspaceId={workspaceId}
            sessionId={tabs.active}
            onSelect={handleSelectSession}
            onSessionDeleted={handleSessionDeleted}
            onNew={handleNewSession}
          />
        )}
      </div>

      {/* 多会话标签栏（浏览器 tab 式）：全关时隐藏，引导页提供新建入口 */}
      {tabs.open.length > 0 && (
        <SessionTabBar
          workspaceId={workspaceId}
          open={tabs.open}
          active={tabs.active}
          onSelect={handleSelectSession}
          onClose={handleCloseTab}
          onNew={handleNewSession}
        />
      )}

      <div className="flex min-h-0 flex-1">
        {/* VS Code 式 Activity Bar（选中会话后可用；workspace 级单实例） */}
        {tabs.active && (
          <ActivityBar
            sessionId={tabs.active}
            workspaceId={workspaceId}
            variant={isDesktop ? 'sidebar' : 'mobile'}
          />
        )}

        {/* 对话区：所有打开的 tab 保持挂载，非激活用 hidden 隐藏（后台流式继续、草稿不丢） */}
        <div className="min-w-0 flex-1">
          {tabs.open.length > 0 ? (
            tabs.open.map((id) => (
              <div key={id} className={id === tabs.active ? 'h-full' : 'hidden'}>
                <ChatStream
                  sessionId={id}
                  workspaceId={workspaceId}
                  model={modelFor(id)}
                  active={id === tabs.active}
                  onModelChange={(m) => setModelOverrides((o) => ({ ...o, [id]: m }))}
                />
              </div>
            ))
          ) : (
            <div className="flex h-full flex-col items-center justify-center gap-3 p-8 text-sm text-muted-foreground">
              <p>{workspaceId ? t('agent.selectOrNewSession') : t('agent.selectWorkspaceFirst')}</p>
              {workspaceId && (
                <Button variant="outline" size="sm" onClick={() => void handleNewSession()}>
                  {t('agent.newSession')}
                </Button>
              )}
            </div>
          )}
        </div>
      </div>

      {showWorkspaceDialog && (
        <WorkspaceDialog
          editing={editingWorkspace ?? undefined}
          onClose={() => {
            setShowWorkspaceDialog(false);
            setEditingWorkspace(null);
          }}
          onCreated={(w) => {
            setWorkspaceId(w.id);
            setShowWorkspaceDialog(false);
            setEditingWorkspace(null);
          }}
        />
      )}
    </div>
  );
}

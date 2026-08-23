import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
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
import { listAgentSelectableModels, resolveWorkspaceModelRef } from '../api/agentModels';
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
  takePendingActivate,
  type TabState,
} from '../components/agent/tabsStore';
import { safeLocalStorageGet, safeLocalStorageSet } from '../components/agent/safeStorage';
import { useMediaQuery } from '../hooks/useMediaQuery';
import { useAgentNotifications } from '../notifications/NotificationProvider';

export default function AgentPage() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  // 上报当前查看的会话：全局通知服务据此判断「用户正盯着该会话」时跳过提醒
  const { setActiveSessionId } = useAgentNotifications();
  // 桌面/移动端分支：ActivityBar 在 <768px 下切到底部图标栏 + Sheet 面板
  const isDesktop = useMediaQuery('(min-width: 768px)');
  const [workspaceId, setWorkspaceId] = useState(
    () => safeLocalStorageGet('agent.lastWorkspaceId') ?? '',
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

  const setTabs = useCallback((updater: (cur: TabState) => TabState) => {
    setTabsByWs((prev) => {
      const cur = prev[workspaceId] ?? { open: [], active: '' };
      return { ...prev, [workspaceId]: updater(cur) };
    });
  }, [workspaceId]);

  // 工作区选中：空态下只有一个 workspace 时自动选中；从 localStorage 恢复的
  // workspaceId 若已不在列表中（被删除/失效），回退到第一个可用工作区，而不是
  // 卡在失效 id（M7）——否则 sessions query 恒为空、顶栏显示无效工作区。
  useEffect(() => {
    if (!workspaces) return;
    if (!workspaceId) {
      if (workspaces.length === 1) setWorkspaceId(workspaces[0].id);
      return;
    }
    if (!workspaces.some((w) => w.id === workspaceId)) {
      setWorkspaceId(workspaces[0]?.id ?? '');
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
    if (workspaceId) safeLocalStorageSet('agent.lastWorkspaceId', workspaceId);
    if (workspaceId && initedWsRef.current[workspaceId]) {
      saveTabs(workspaceId, tabs);
    }
  }, [workspaceId, tabs]);

  // 通知点击定位：消费「待激活会话」（NotificationProvider 写入）。workspaces 已载
  // 即切到目标工作区；该工作区 sessions 到达（且 tabs 初始化完成）后把会话并入
  // tabs 并激活。pendingRef 跨渲染持有：takePendingActivate 是一次性消费，而工作区
  // 切换→sessions 到达是异步两步。
  const pendingActivateRef = useRef<{ workspaceId: string; sessionId: string } | null>(null);
  useEffect(() => {
    if (!workspaces) return;
    const pending = (pendingActivateRef.current ??= takePendingActivate());
    if (!pending) return;
    if (!workspaces.some((w) => w.id === pending.workspaceId)) {
      // 目标工作区已被删除：无可定位，丢弃。
      pendingActivateRef.current = null;
      return;
    }
    if (workspaceId !== pending.workspaceId) {
      setWorkspaceId(pending.workspaceId);
      return; // sessions 由下一个 effect 周期到达
    }
    if (!sessions || !initedWsRef.current[workspaceId]) return;
    pendingActivateRef.current = null;
    if (sessions.some((s) => s.id === pending.sessionId)) {
      const wsId = pending.workspaceId;
      setTabsByWs((prev) => {
        const cur = prev[wsId] ?? { open: [], active: '' };
        return { ...prev, [wsId]: openOrActivate(cur, pending.sessionId) };
      });
    }
    // 会话已被删除（sessions 里没有）：静默丢弃，停留当前 tab。
  }, [workspaces, workspaceId, sessions]);

  // 会话模型派生：tab 局部覆盖优先；否则按「会话模型 → workspace 默认 →
  // 全局默认 → 第一个可用模型」的 falsy 链回退（?? 与 || 混合处加括号，避免
  // 语法错误/歧义）。workspace 层（M11）：后端按 session→workspace→全局 解析，
  // 前端链此前缺 workspace 层，会把全局默认/首个可用误当 workspace 默认展示、
  // 并在新建会话时静默覆盖 workspace 级默认模型。
  const currentWorkspace = workspaces?.find((w) => w.id === workspaceId);
  const workspaceModel = useMemo(
    () => resolveWorkspaceModelRef(currentWorkspace?.llm_model_id, selectableModels),
    [currentWorkspace?.llm_model_id, selectableModels],
  );
  // 会话 id → 模型的反向索引：sessions 数组变化时一次性构建，避免 modelFor 内
  // 每次 find 扫描（O(n) → O(1)），也让 modelFor 的依赖稳定（Map 引用仅随
  // sessions 变化）。
  const sessionModelMap = useMemo(() => {
    const m = new Map<string, string>();
    for (const s of sessions ?? []) {
      const v = (s.model ?? '').trim();
      if (v) m.set(s.id, v);
    }
    return m;
  }, [sessions]);
  const fallbackModel = useMemo(
    () =>
      workspaceModel ||
      defaultModel?.trim() ||
      selectableModels?.models[0]?.id ||
      selectableModels?.groups[0]?.id ||
      '',
    [workspaceModel, defaultModel, selectableModels],
  );
  const modelFor = useCallback(
    (sid: string) => modelOverrides[sid] ?? sessionModelMap.get(sid) ?? fallbackModel,
    [modelOverrides, sessionModelMap, fallbackModel],
  );

  // 上报当前查看的会话（激活标签页）给全局通知服务；离开 Agent 页（卸载）时清空，
  // 让其它会话的任务完成/需干预事件能正常提醒。
  useEffect(() => {
    setActiveSessionId(tabs.active || null);
    return () => setActiveSessionId(null);
  }, [tabs.active, setActiveSessionId]);

  // 删除会话：任意会话被删都关掉对应标签（若已打开）
  const handleSessionDeleted = useCallback((id: string) => {
    setTabs((cur) => closeTab(cur, id));
  }, [setTabs]);

  const handleSelectWorkspace = useCallback((id: string) => {
    setWorkspaceId(id);
  }, []);

  // 点击标签 / SessionBar 选择会话 → 打开或激活对应 tab
  const handleSelectSession = useCallback((id: string) => {
    setTabs((cur) => openOrActivate(cur, id));
  }, [setTabs]);

  // 关闭标签：仅关标签，会话数据保留（SessionBar 下拉仍可重新打开）
  const handleCloseTab = useCallback((id: string) => {
    setTabs((cur) => closeTab(cur, id));
  }, [setTabs]);

  const handleNewSession = useCallback(async () => {
    if (!workspaceId) return;
    // 仅在用户显式选择过模型（tab 记忆）时继承；否则不落库 model，交由后端按
    // session→workspace→全局 链解析（M11）——把前端推导值（workspace 默认/
    // 全局默认/首个可用）持久化为 session.model 会静默覆盖 workspace 级默认模型，
    // 且 workspace 默认模型后续变更时旧会话无法跟随。
    const explicit = modelOverrides[tabs.active];
    const s = await createAgentSession(workspaceId, undefined, explicit || undefined);
    // 同步写入共享缓存：让列表/标签栏立即可见新会话（无需等 invalidate）
    queryClient.setQueryData<AgentSession[]>(['agent-sessions', workspaceId], (old) => [
      s,
      ...(old ?? []),
    ]);
    setTabs((cur) => openOrActivate(cur, s.id));
  }, [workspaceId, modelOverrides, tabs.active, queryClient, setTabs]);

  // 齿轮入口：打开编辑模式的 WorkspaceDialog（预填当前工作区，client/运行时不可改）
  const openEditWorkspace = useCallback(() => {
    const w = workspaces?.find((x) => x.id === workspaceId) ?? null;
    setEditingWorkspace(w);
    setShowWorkspaceDialog(true);
  }, [workspaces, workspaceId]);

  const workspaceApprovalMode = useMemo(
    () => workspaces?.find((w) => w.id === workspaceId)?.approval_mode ?? 'safe',
    [workspaces, workspaceId],
  );

  const handleModelChangeFor = useCallback(
    (sid: string) => (m: string) => setModelOverrides((o) => ({ ...o, [sid]: m })),
    [],
  );

  // 高度由外层布局决定：AppLayout 对 /agent 路由走非滚动分支（h-dvh → flex-1 →
  // h-full 高度链），本页用 h-full 精确填满 Header 以下剩余空间。此前用
  // calc(100dvh-…) 视口单位拼凑高度，移动端 100dvh 与外层 100vh 不一致（地址栏
  // 动态变化）会导致 AgentPage 高出外层剩余空间 → 外层 ScrollArea 与消息区叠出
  // 双重滚动条（历史：5ad703a 修过一次仍复发）。
  return (
    <div className="flex h-full min-h-[320px] flex-col overflow-hidden rounded-xl border border-border/70 bg-card md:min-h-[480px]">
      {/* 顶栏：logo + WorkspaceBar + SessionBar + 多会话标签（同一行，省空间；
          标签区 flex-1 横向滚动，全关时隐藏，引导页提供新建入口） */}
      <div className="flex items-center gap-1.5 border-b border-border/60 p-1.5 md:gap-2 md:p-2">
        {/* 移动端 393px 宽度寸土寸金：装饰图标让位给标签栏，仅桌面端显示 */}
        <Sparkles className="hidden h-4 w-4 shrink-0 text-primary md:block" />
        <WorkspaceBar
          workspaceId={workspaceId}
          onSelect={handleSelectWorkspace}
          onNew={() => setShowWorkspaceDialog(true)}
          onEdit={openEditWorkspace}
        />
        {/* SessionBar 仅桌面端：移动端由 SessionTabBar 的会话标题承担点击打开
            同一会话下拉的职责（标题即按钮），不再重复渲染这个图标按钮。 */}
        {workspaceId && (
          <div className="hidden md:contents">
            <SessionBar
              workspaceId={workspaceId}
              sessionId={tabs.active}
              onSelect={handleSelectSession}
              onSessionDeleted={handleSessionDeleted}
              onNew={handleNewSession}
            />
          </div>
        )}
        {tabs.open.length > 0 && (
          <SessionTabBar
            workspaceId={workspaceId}
            open={tabs.open}
            active={tabs.active}
            onSelect={handleSelectSession}
            onClose={handleCloseTab}
            onNew={handleNewSession}
            onSessionDeleted={handleSessionDeleted}
          />
        )}
      </div>

      <div className="flex min-h-0 flex-1">
        {/* VS Code 式 Activity Bar（选中会话后可用；workspace 级单实例）。
            桌面端：侧栏在横向 flex 内；移动端：footer 行在对话区下方（见下方
            第二个渲染点），与顶栏上下对称、贴住聊天区无空隙。 */}
        {tabs.active && isDesktop && (
          <ActivityBar
            sessionId={tabs.active}
            workspaceId={workspaceId}
            variant="sidebar"
          />
        )}

        {/* 对话区：所有打开的 tab 保持挂载，非激活用 hidden 隐藏（后台流式继续、草稿不丢）。 */}
        <div className="min-w-0 flex-1">
          {tabs.open.length > 0 ? (
            tabs.open.map((id) => (
              <div key={id} className={id === tabs.active ? 'h-full' : 'hidden'}>
                <ChatStream
                  sessionId={id}
                  workspaceId={workspaceId}
                  model={modelFor(id)}
                  approvalMode={workspaceApprovalMode}
                  active={id === tabs.active}
                  onModelChange={handleModelChangeFor(id)}
                  claudeTierModels={currentWorkspace?.claude_tier_models}
                  agentType={currentWorkspace?.agent_type}
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

      {/* 移动端底栏：卡片内 footer（border-t 贴住对话区下缘，与顶栏对称）。
          输入框随聊天区下移到本栏正上方，中间无空隙。 */}
      {tabs.active && !isDesktop && (
        <ActivityBar
          sessionId={tabs.active}
          workspaceId={workspaceId}
          variant="mobile"
        />
      )}

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

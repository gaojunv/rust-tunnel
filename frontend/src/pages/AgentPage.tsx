import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { Sparkles } from 'lucide-react';
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
import ActivityBar from '../components/agent/ActivityBar';
import WorkspaceDialog from '../components/agent/WorkspaceDialog';
import { useMediaQuery } from '../hooks/useMediaQuery';

export default function AgentPage() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  // 桌面/移动端分支：ActivityBar 在 <768px 下切到底部图标栏 + Sheet 面板
  const isDesktop = useMediaQuery('(min-width: 768px)');
  const [workspaceId, setWorkspaceId] = useState(
    () => localStorage.getItem('agent.lastWorkspaceId') ?? '',
  );
  const [sessionId, setSessionId] = useState(
    () => localStorage.getItem('agent.lastSessionId') ?? '',
  );
  const [model, setModel] = useState('');
  const [showWorkspaceDialog, setShowWorkspaceDialog] = useState(false);
  // 编辑模式：传入当前工作区则 WorkspaceDialog 走 PUT（client/运行时不可改）
  const [editingWorkspace, setEditingWorkspace] = useState<AgentWorkspace | null>(null);
  // 自动选中守卫：切换 workspace / 新建会话 / 手动选择后允许自动选中最近会话；
  // 删除当前会话后置 false，严格回引导态（不自动重选），直到用户再次手动选择或切 workspace。
  const autoSelectRef = useRef(true);

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

  // 只有一个 workspace 时自动选中
  useEffect(() => {
    if (!workspaceId && workspaces?.length === 1) {
      setWorkspaceId(workspaces[0].id);
    }
  }, [workspaces, workspaceId]);

  // 选中 workspace 后自动选中最近会话（sessions 已按 created_at DESC 排序）
  useEffect(() => {
    if (!workspaceId || !sessions) return;
    // 删除当前会话后回引导态：显式清空（删除）后禁止自动重选，直到手动选择/切 workspace
    if (!autoSelectRef.current) return;
    if (sessions.length === 0) {
      setSessionId('');
      return;
    }
    if (!sessions.some((s) => s.id === sessionId)) {
      setSessionId(sessions[0].id);
    }
  }, [sessions, workspaceId, sessionId]);

  // 会话切换：回显其模型（空则回退全局默认，再空则回退第一个可用模型，与后端一致）
  useEffect(() => {
    const cur = sessions?.find((s) => s.id === sessionId);
    const sessionModel = cur?.model?.trim();
    const globalDefault = defaultModel?.trim();
    const fallback =
      selectableModels?.models[0]?.id || selectableModels?.groups[0]?.id || '';
    setModel(sessionModel || globalDefault || fallback);
  }, [sessionId, sessions, defaultModel, selectableModels]);

  const handleNewSession = async () => {
    if (!workspaceId) return;
    const s = await createAgentSession(workspaceId, undefined, model || undefined);
    // 同步写入共享缓存：确保自动选中 effect 不会因陈旧列表把新会话打回旧会话
    queryClient.setQueryData<AgentSession[]>(['agent-sessions', workspaceId], (old) => [
      s,
      ...(old ?? []),
    ]);
    autoSelectRef.current = true;
    setSessionId(s.id);
  };

  const handleSelectWorkspace = (id: string) => {
    // 切换 workspace → 重新允许自动选中最近会话
    autoSelectRef.current = true;
    setWorkspaceId(id);
    setSessionId('');
  };

  // 手动选择会话 → 重新允许自动选中（后续列表变更时按需自愈）
  const handleSelectSession = (id: string) => {
    autoSelectRef.current = true;
    setSessionId(id);
  };

  // 删除当前会话 → 严格回引导态：清空选中且禁止自动重选
  const handleDeletedCurrent = () => {
    autoSelectRef.current = false;
    setSessionId('');
  };

  // 刷新恢复：选中变化即持久化，F5 后回到刷新前的 workspace/session
  useEffect(() => {
    if (workspaceId) localStorage.setItem('agent.lastWorkspaceId', workspaceId);
    localStorage.setItem('agent.lastSessionId', sessionId);
  }, [workspaceId, sessionId]);

  // 齿轮入口：打开编辑模式的 WorkspaceDialog（预填当前工作区，client/运行时不可改）
  const openEditWorkspace = () => {
    const w = workspaces?.find((x) => x.id === workspaceId) ?? null;
    setEditingWorkspace(w);
    setShowWorkspaceDialog(true);
  };

  return (
    <div className="flex h-[calc(100dvh-7.5rem)] min-h-[320px] flex-col overflow-hidden rounded-xl border border-border/70 bg-card md:min-h-[480px]">
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
            sessionId={sessionId}
            onSelect={handleSelectSession}
            onDeletedCurrent={handleDeletedCurrent}
            onNew={handleNewSession}
          />
        )}
      </div>

      <div className="flex min-h-0 flex-1">
        {/* VS Code 式 Activity Bar（选中会话后可用） */}
        {sessionId && (
          <ActivityBar
            sessionId={sessionId}
            workspaceId={workspaceId}
            variant={isDesktop ? 'sidebar' : 'mobile'}
          />
        )}

        {/* 对话区 */}
        <div className="min-w-0 flex-1">
          {sessionId ? (
            <ChatStream
              key={sessionId}
              sessionId={sessionId}
              workspaceId={workspaceId}
              model={model}
              onModelChange={setModel}
            />
          ) : (
            <div className="flex h-full items-center justify-center p-8 text-sm text-muted-foreground">
              {workspaceId ? t('agent.selectOrNewSession') : t('agent.selectWorkspaceFirst')}
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
            // 编辑模式保留当前会话（只是改设置）；新建才回到引导态
            if (!editingWorkspace) setSessionId('');
            setShowWorkspaceDialog(false);
            setEditingWorkspace(null);
          }}
        />
      )}
    </div>
  );
}

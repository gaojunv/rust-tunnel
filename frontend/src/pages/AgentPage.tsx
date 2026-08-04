import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { Sparkles } from 'lucide-react';
import {
  listAgentWorkspaces,
  listAgentSessions,
  createAgentSession,
  getAgentDefaultModel,
} from '../api/client';
import type { AgentWorkspace, AgentSession } from '../types';
import ChatStream from '../components/agent/ChatStream';
import WorkspaceBar from '../components/agent/WorkspaceBar';
import SessionBar from '../components/agent/SessionBar';
import ActivityBar from '../components/agent/ActivityBar';
import WorkspaceDialog from '../components/agent/WorkspaceDialog';

export default function AgentPage() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [workspaceId, setWorkspaceId] = useState('');
  const [sessionId, setSessionId] = useState('');
  const [model, setModel] = useState('');
  const [showWorkspaceDialog, setShowWorkspaceDialog] = useState(false);

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

  // 只有一个 workspace 时自动选中
  useEffect(() => {
    if (!workspaceId && workspaces?.length === 1) {
      setWorkspaceId(workspaces[0].id);
    }
  }, [workspaces, workspaceId]);

  // 选中 workspace 后自动选中最近会话（sessions 已按 created_at DESC 排序）
  useEffect(() => {
    if (!workspaceId || !sessions) return;
    if (sessions.length === 0) {
      setSessionId('');
      return;
    }
    if (!sessions.some((s) => s.id === sessionId)) {
      setSessionId(sessions[0].id);
    }
  }, [sessions, workspaceId, sessionId]);

  // 会话切换：回显其模型（空则回退全局默认）
  useEffect(() => {
    const cur = sessions?.find((s) => s.id === sessionId);
    setModel(cur?.model || defaultModel || '');
  }, [sessionId, sessions, defaultModel]);

  const handleNewSession = async () => {
    if (!workspaceId) return;
    const s = await createAgentSession(workspaceId, undefined, model || undefined);
    // 同步写入共享缓存：确保自动选中 effect 不会因陈旧列表把新会话打回旧会话
    queryClient.setQueryData<AgentSession[]>(['agent-sessions', workspaceId], (old) => [
      s,
      ...(old ?? []),
    ]);
    setSessionId(s.id);
  };

  const handleSelectWorkspace = (id: string) => {
    setWorkspaceId(id);
    setSessionId('');
  };

  return (
    <div className="flex h-[calc(100dvh-7.5rem)] min-h-[480px] flex-col overflow-hidden rounded-xl border border-border/70 bg-card">
      {/* 顶栏：logo + WorkspaceBar + SessionBar */}
      <div className="flex items-center gap-2 border-b border-border/60 p-2">
        <Sparkles className="h-4 w-4 shrink-0 text-primary" />
        <WorkspaceBar
          workspaceId={workspaceId}
          onSelect={handleSelectWorkspace}
          onNew={() => setShowWorkspaceDialog(true)}
        />
        {workspaceId && (
          <SessionBar
            workspaceId={workspaceId}
            sessionId={sessionId}
            onSelect={setSessionId}
            onNew={handleNewSession}
          />
        )}
      </div>

      <div className="flex min-h-0 flex-1">
        {/* VS Code 式 Activity Bar（选中会话后可用） */}
        {sessionId && <ActivityBar sessionId={sessionId} />}

        {/* 对话区 */}
        <div className="min-w-0 flex-1">
          {sessionId ? (
            <ChatStream
              key={sessionId}
              sessionId={sessionId}
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
          onClose={() => setShowWorkspaceDialog(false)}
          onCreated={(w) => {
            setWorkspaceId(w.id);
            setSessionId('');
            setShowWorkspaceDialog(false);
          }}
        />
      )}
    </div>
  );
}

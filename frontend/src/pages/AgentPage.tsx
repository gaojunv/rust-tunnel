import { useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { Button } from '@/components/ui/button';
import { PanelLeft, Plus, Sparkles } from 'lucide-react';
import {
  listAgentWorkspaces,
  listAgentSessions,
  createAgentSession,
} from '../api/client';
import type { AgentWorkspace, AgentSession } from '../types';
import ChatStream from '../components/agent/ChatStream';
import SidebarTabs from '../components/agent/SidebarTabs';
import SessionList from '../components/agent/SessionList';
import WorkspaceDialog from '../components/agent/WorkspaceDialog';

export default function AgentPage() {
  const [workspaceId, setWorkspaceId] = useState('');
  const [sessionId, setSessionId] = useState('');
  const [sidebarOpen, setSidebarOpen] = useState(true);
  const [showWorkspaceDialog, setShowWorkspaceDialog] = useState(false);

  const { data: workspaces } = useQuery<AgentWorkspace[]>({
    queryKey: ['agent-workspaces'],
    queryFn: listAgentWorkspaces,
  });

  const { data: sessions, refetch: refetchSessions } = useQuery<AgentSession[]>({
    queryKey: ['agent-sessions', workspaceId],
    queryFn: () => listAgentSessions(workspaceId),
    enabled: !!workspaceId,
  });

  const handleNewSession = async () => {
    if (!workspaceId) return;
    const s = await createAgentSession(workspaceId);
    setSessionId(s.id);
    refetchSessions();
  };

  return (
    <div className="flex h-[calc(100dvh-7.5rem)] min-h-[480px] flex-col overflow-hidden rounded-xl border border-border/70 bg-card">
      {/* 顶栏 */}
      <div className="flex items-center gap-2 border-b border-border/60 p-2">
        <Sparkles className="h-4 w-4 shrink-0 text-primary" />
        <select
          value={workspaceId}
          onChange={(e) => {
            setWorkspaceId(e.target.value);
            setSessionId('');
          }}
          className="h-9 max-w-[220px] rounded-md border border-input bg-background px-3 py-1 text-sm"
          aria-label="选择工作区"
        >
          <option value="">选择工作区…</option>
          {workspaces?.map((w) => (
            <option key={w.id} value={w.id}>
              {w.name}
            </option>
          ))}
        </select>
        <Button variant="outline" size="sm" onClick={() => setShowWorkspaceDialog(true)}>
          <Plus className="mr-1 h-4 w-4" />
          工作区
        </Button>
        {workspaceId && (
          <Button variant="outline" size="sm" onClick={handleNewSession}>
            <Plus className="mr-1 h-4 w-4" />
            新会话
          </Button>
        )}
        <Button
          variant="ghost"
          size="sm"
          className="ml-auto"
          onClick={() => setSidebarOpen((o) => !o)}
        >
          <PanelLeft className="mr-1 h-4 w-4" />
          {sidebarOpen ? '收起侧栏' : '展开侧栏'}
        </Button>
      </div>

      <div className="flex min-h-0 flex-1">
        {/* 左侧栏：会话列表 + 侧栏 tabs */}
        {sidebarOpen && workspaceId && sessionId && (
          <div className="flex w-72 shrink-0 flex-col border-r border-border/60">
            <SessionList sessions={sessions ?? []} activeId={sessionId} onSelect={setSessionId} />
            <SidebarTabs workspaceId={workspaceId} sessionId={sessionId} />
          </div>
        )}

        {/* 对话区 */}
        <div className="min-w-0 flex-1">
          {sessionId ? (
            <ChatStream key={sessionId} sessionId={sessionId} />
          ) : (
            <div className="flex h-full items-center justify-center p-8 text-sm text-muted-foreground">
              {workspaceId ? '选择或新建一个会话开始' : '先选择一个工作区'}
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

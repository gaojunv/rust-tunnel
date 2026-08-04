import { useTranslation } from 'react-i18next';
import { useQuery } from '@tanstack/react-query';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { listAgentMessages } from '../../api/client';
import type { AgentMessage } from '../../types';

interface Props {
  workspaceId: string;
  sessionId: string;
}

interface ToolLog {
  name: string;
  args: string;
  result: string;
}

export default function SidebarTabs({ sessionId }: Props) {
  const { t } = useTranslation();
  // 与 ChatStream 共享 queryKey：ChatStream 在回合结束后 invalidate，此处自动刷新
  const { data: messages } = useQuery<AgentMessage[]>({
    queryKey: ['agent-messages', sessionId],
    queryFn: () => listAgentMessages(sessionId),
  });

  // 从对话历史解析最近一次 git_status 工具结果
  let latestGitStatus: string | null = null;
  for (const m of messages ?? []) {
    if (m.role === 'tool' && m.tool_calls) {
      try {
        const logs = JSON.parse(m.tool_calls) as ToolLog[];
        for (const log of logs) {
          if (log.name === 'git_status') latestGitStatus = log.result;
        }
      } catch {
        /* ignore malformed tool_calls */
      }
    }
  }

  return (
    <div className="flex-1 overflow-y-auto p-2">
      <Tabs defaultValue="git">
        <TabsList className="w-full">
          <TabsTrigger value="files" className="flex-1">
            {t('agent.files')}
          </TabsTrigger>
          <TabsTrigger value="terminal" className="flex-1">
            {t('agent.terminal')}
          </TabsTrigger>
          <TabsTrigger value="git" className="flex-1">
            {t('agent.git')}
          </TabsTrigger>
        </TabsList>
        <TabsContent value="files" className="mt-2 text-xs text-muted-foreground">
          {t('agent.filesComingSoon')}
        </TabsContent>
        <TabsContent value="terminal" className="mt-2 text-xs text-muted-foreground">
          {t('agent.terminalComingSoon')}
        </TabsContent>
        <TabsContent value="git" className="mt-2">
          {latestGitStatus ? (
            <pre className="whitespace-pre-wrap rounded-md bg-muted p-2 font-mono text-xs">
              {latestGitStatus}
            </pre>
          ) : (
            <p className="text-xs text-muted-foreground">
              {t('agent.noGitStatus')}
            </p>
          )}
        </TabsContent>
      </Tabs>
    </div>
  );
}

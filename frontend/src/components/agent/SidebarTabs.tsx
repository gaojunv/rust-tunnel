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
            文件
          </TabsTrigger>
          <TabsTrigger value="terminal" className="flex-1">
            终端
          </TabsTrigger>
          <TabsTrigger value="git" className="flex-1">
            Git
          </TabsTrigger>
        </TabsList>
        <TabsContent value="files" className="mt-2 text-xs text-muted-foreground">
          文件树面板将在后续版本提供。Agent 的 ls / 文件操作输出已显示在对话流中。
        </TabsContent>
        <TabsContent value="terminal" className="mt-2 text-xs text-muted-foreground">
          终端面板将在后续版本提供。命令输出已显示在对话流中。
        </TabsContent>
        <TabsContent value="git" className="mt-2">
          {latestGitStatus ? (
            <pre className="whitespace-pre-wrap rounded-md bg-muted p-2 font-mono text-xs">
              {latestGitStatus}
            </pre>
          ) : (
            <p className="text-xs text-muted-foreground">
              暂无 git status 结果。让 Agent 运行 git status 后在此查看。
            </p>
          )}
        </TabsContent>
      </Tabs>
    </div>
  );
}

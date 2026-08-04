import { useTranslation } from 'react-i18next';
import { useQuery } from '@tanstack/react-query';
import { listAgentMessages } from '../../../api/client';
import type { AgentMessage } from '../../../types';

interface ToolLog {
  name: string;
  args: string;
  result: string;
}

export default function GitPanel({ sessionId }: { sessionId: string }) {
  const { t } = useTranslation();
  // 与 ChatStream 共享 queryKey：ChatStream 回合结束 invalidate 后此处自动刷新
  const { data: messages } = useQuery<AgentMessage[]>({
    queryKey: ['agent-messages', sessionId],
    queryFn: () => listAgentMessages(sessionId),
  });

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

  return latestGitStatus ? (
    <pre className="whitespace-pre-wrap rounded-md bg-muted p-2 font-mono text-xs">
      {latestGitStatus}
    </pre>
  ) : (
    <p className="text-xs text-muted-foreground">{t('agent.noGitStatus')}</p>
  );
}

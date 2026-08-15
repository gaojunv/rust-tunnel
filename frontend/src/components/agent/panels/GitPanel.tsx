import { useState } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import {
  getAgentGitStatus,
  listAgentMessages,
} from '../../../api/client';
import type { AgentMessagesPage } from '../../../api/client';
import type { AgentMessage } from '../../../types';
import { GitChangesTab } from './git/ChangesTab';
import { GitBranchesTab } from './git/BranchesTab';
import { GitHistoryTab } from './git/HistoryTab';
import { GitStashTab } from './git/StashTab';
import { GitTabBar, GitToolbar } from './git/shared';
import {
  headerBranch,
  parsePorcelainEntries,
  type GitEntry,
  type GitStatusKind,
} from './git/gitUtils';
import { parseToolResultContent } from '../types';

// 向后兼容导出（gitUtils.ts 内为唯一实现，GitPanel 仅转发，供既有测试/调用方使用）
export type { GitEntry, GitStatusKind };
export { parsePorcelainEntries };

type TabKey = 'changes' | 'branches' | 'history' | 'stash';

type TabI18nKey =
  | 'agent.gitTabChanges'
  | 'agent.gitTabBranches'
  | 'agent.gitTabHistory'
  | 'agent.gitTabStash';

const TABS: { key: TabKey; i18nKey: TabI18nKey }[] = [
  { key: 'changes', i18nKey: 'agent.gitTabChanges' },
  { key: 'branches', i18nKey: 'agent.gitTabBranches' },
  { key: 'history', i18nKey: 'agent.gitTabHistory' },
  { key: 'stash', i18nKey: 'agent.gitTabStash' },
];

interface ToolLog {
  name: string;
  args: string;
  result: string;
}

function latestGitStatus(messages: AgentMessage[]): string | null {
  let latest: string | null = null;
  for (const m of messages ?? []) {
    if (m.kind === 'tool_result' && m.name === 'git_status') {
      // 服务端新契约：tool_result content 可能是 JSON {text,...}（存量旧行为纯文本）
      latest = parseToolResultContent(m.content).text;
    } else if ((m.kind === 'tool' || m.role === 'tool') && m.tool_calls) {
      // 旧格式兼容
      try {
        const logs = JSON.parse(m.tool_calls) as ToolLog[];
        for (const log of logs) {
          if (log.name === 'git_status') latest = log.result;
        }
      } catch {
        /* ignore malformed tool_calls */
      }
    }
  }
  return latest;
}

function FallbackGitStatus({ messages }: { messages: AgentMessage[] }) {
  const { t } = useTranslation();
  const latest = latestGitStatus(messages);
  return latest ? (
    <pre className="whitespace-pre-wrap rounded-md bg-muted p-2 font-mono text-xs">
      {latest}
    </pre>
  ) : (
    <p className="text-xs text-muted-foreground">{t('agent.noGitStatus')}</p>
  );
}

/**
 * Git 面板（ActivityBar kind='git'）：tab 容器——Changes / Branches / History / Stash。
 *
 * status query 归属本容器：非 git 仓库（stderr 非空）与回退路径（主 API 离线）统一
 * 在此判定，不随 tab 切换丢失。四个 tab 各自维护自己的查询与写操作，写成功后
 * invalidate `agent-git-status`（本容器持有的 query）实现跨 tab 联动刷新。
 */
export default function GitPanel({
  sessionId,
  workspaceId,
}: {
  sessionId: string;
  workspaceId: string;
}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [tab, setTab] = useState<TabKey>('changes');

  const statusQuery = useQuery({
    queryKey: ['agent-git-status', workspaceId],
    queryFn: () => getAgentGitStatus(workspaceId),
    retry: false,
  });

  // 仅回退路径需要消息：主 API 不可用（如客户端离线 503）时才拉取。
  // 分页上限传 500 尽量覆盖 git_status 结果。注意：不能用 ['agent-messages',
  // sessionId] 裸 key——ChatStream 的同一 key 拉的是默认 200 条、has_more 语义
  // 不同，两者共享缓存槽会互相覆盖、破坏「加载更早」状态。用带子键的独立 key：
  // 缓存互相隔离，但 ChatStream done/重连的 ['agent-messages', sessionId] 前缀
  // invalidate 仍能命中并联动刷新。
  const messagesQuery = useQuery<AgentMessagesPage>({
    queryKey: ['agent-messages', sessionId, 'git-fallback'],
    queryFn: () => listAgentMessages(sessionId, { limit: 500 }),
    enabled: statusQuery.isError,
    retry: false,
  });

  // 全局刷新：一次 invalidate 所有 git 查询（各 tab 懒挂载的 query 也会标记 stale，
  // 下次挂载时自动重取），避免每个 tab 各自维护一个工具栏。
  const refresh = () => {
    queryClient.invalidateQueries({ queryKey: ['agent-git-status'] });
    queryClient.invalidateQueries({ queryKey: ['agent-git-diff'] });
    queryClient.invalidateQueries({ queryKey: ['agent-git-branches'] });
    queryClient.invalidateQueries({ queryKey: ['agent-git-log'] });
    queryClient.invalidateQueries({ queryKey: ['agent-git-show'] });
    queryClient.invalidateQueries({ queryKey: ['agent-git-stash'] });
  };

  // 回退：保留旧行为——展示缓存里最近一次 git_status 工具结果原文
  if (statusQuery.isError) {
    return (
      <div className="overflow-y-auto p-2">
        <FallbackGitStatus messages={messagesQuery.data?.messages ?? []} />
      </div>
    );
  }

  if (statusQuery.isLoading) {
    return (
      <div className="px-1 py-2 text-xs text-muted-foreground">{t('common.loading')}</div>
    );
  }

  const { status, stderr } = statusQuery.data ?? { status: '', stderr: '' };

  if (status.trim() === '' && stderr.trim() !== '') {
    return (
      <div className="space-y-2">
        <GitToolbar onRefresh={refresh} refreshLabel={t('agent.refresh')} />
        <p className="px-1 text-xs text-muted-foreground">{t('agent.notGitRepo')}</p>
      </div>
    );
  }

  const entries = parsePorcelainEntries(status);
  const branch = headerBranch(status);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <GitToolbar onRefresh={refresh} refreshLabel={t('agent.refresh')} />
      <GitTabBar
        tabs={TABS.map((item) => ({ key: item.key, label: t(item.i18nKey) }))}
        active={tab}
        onChange={setTab}
      />
      <div className="min-h-0 flex-1 overflow-y-auto p-2">
        {tab === 'changes' && (
          <GitChangesTab workspaceId={workspaceId} entries={entries} branch={branch} />
        )}
        {tab === 'branches' && <GitBranchesTab workspaceId={workspaceId} />}
        {tab === 'history' && <GitHistoryTab workspaceId={workspaceId} />}
        {tab === 'stash' && <GitStashTab workspaceId={workspaceId} />}
      </div>
    </div>
  );
}

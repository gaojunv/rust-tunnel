import { useState } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { getAgentGithubRepo, getAgentGithubRuns, getAgentGithubWorkflows } from '../../../../api/client';
import { GitTabBar, GitToolbar } from '../git/shared';
import { isRunActive } from './githubUtils';
import { RunsTab } from './RunsTab';
import { WorkflowsTab } from './WorkflowsTab';
import { GithubErrorBanner } from './shared';

type TabKey = 'runs' | 'workflows';

const TABS: { key: TabKey; i18nKey: 'agent.githubTabRuns' | 'agent.githubTabWorkflows' }[] = [
  { key: 'runs', i18nKey: 'agent.githubTabRuns' },
  { key: 'workflows', i18nKey: 'agent.githubTabWorkflows' },
];

/**
 * GitHub Actions 面板（ActivityBar kind='github'）：tab 容器——Runs / Workflows。
 *
 * repo 定位 query 归属本容器（后端 `/github/repo`：token 布尔位 + 手填/隧道探测的
 * owner/repo），据此裁决空态优先级：token 未配置 → 引导去 workspace 设置；配置了
 * 但探测不到仓库 → 提示手填 + 强制重探。runs query 也归本容器，保证跨 tab 轮询
 * （有进行中 run 时 10s，否则 30s）；工作流过滤下拉的状态一并留在容器。
 */
export default function GitHubActionsPanel({ workspaceId }: { workspaceId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [tab, setTab] = useState<TabKey>('runs');
  const [workflowFilter, setWorkflowFilter] = useState('');

  const repoQuery = useQuery({
    queryKey: ['agent-github-repo', workspaceId],
    queryFn: () => getAgentGithubRepo(workspaceId),
    retry: false,
  });

  const repo = repoQuery.data;
  const tokenSet = repo?.token_set === true;
  const configured = repo?.configured === true;
  // 仅当仓库已定位且 token 已配置时才去拉工作流/运行；否则直接进空态
  const enabled = configured && tokenSet;

  const workflowsQuery = useQuery({
    queryKey: ['agent-github-workflows', workspaceId],
    queryFn: () => getAgentGithubWorkflows(workspaceId),
    enabled,
    retry: false,
  });

  const runsQuery = useQuery({
    queryKey: ['agent-github-runs', workspaceId, workflowFilter],
    queryFn: () =>
      getAgentGithubRuns(workspaceId, {
        ...(workflowFilter !== '' ? { workflow_id: workflowFilter } : {}),
        per_page: 30,
      }),
    enabled,
    retry: false,
    // 轮询：有进行中的运行 → 10s 追踪状态；否则 30s 低频保活
    refetchInterval: (query) => {
      const runs = query.state.data?.workflow_runs;
      const active = runs?.some((r) => isRunActive(r.status)) ?? false;
      return active ? 10_000 : 30_000;
    },
  });

  /** 写操作（rerun/cancel/dispatch）成功后刷新 runs 列表（新运行/状态推进）。 */
  const invalidateRuns = () => {
    queryClient.invalidateQueries({ queryKey: ['agent-github-runs', workspaceId] });
  };

  /** 空态「强制重探」：经隧道重新探测 git remote（后端 5 分钟缓存 + refresh=true 穿透）。 */
  const reprobe = () => {
    queryClient.fetchQuery({
      queryKey: ['agent-github-repo', workspaceId],
      queryFn: () => getAgentGithubRepo(workspaceId, true),
    });
  };

  if (repoQuery.isLoading) {
    return <div className="px-1 py-2 text-xs text-muted-foreground">{t('common.loading')}</div>;
  }

  // repo 检测端点自身失败（503/500/404 等非空态）：直接展示错误
  if (repoQuery.isError) {
    return (
      <div className="space-y-2 p-1">
        <GitToolbar title={t('agent.github')} onRefresh={reprobe} refreshLabel={t('agent.refresh')} />
        <GithubErrorBanner error={repoQuery.error} />
      </div>
    );
  }

  // 空态 1：token 未配置（最高优先级）——引导去 workspace 设置；刷新供配置后重新检测
  if (!tokenSet) {
    return (
      <div className="space-y-2 p-1">
        <GitToolbar title={t('agent.github')} onRefresh={reprobe} refreshLabel={t('agent.refresh')} />
        <p className="px-1 text-xs text-muted-foreground">{t('agent.githubNoToken')}</p>
      </div>
    );
  }

  // 空态 2：token 已配置但仓库未定位（手填 + 隧道探测都失败）——提示手填 + 强制重探
  if (!configured) {
    return (
      <div className="space-y-2 p-1">
        <GitToolbar title={t('agent.github')} onRefresh={reprobe} refreshLabel={t('agent.refresh')} />
        <p className="px-1 text-xs text-muted-foreground">{t('agent.githubNoRepo')}</p>
      </div>
    );
  }

  // 正常态：双 tab（带刷新按钮）
  return (
    <div className="flex h-full min-h-0 flex-col">
      <GitToolbar title={t('agent.github')} onRefresh={reprobe} refreshLabel={t('agent.refresh')} />
      <GitTabBar
        tabs={TABS.map((item) => ({ key: item.key, label: t(item.i18nKey) }))}
        active={tab}
        onChange={setTab}
      />
      <div className="min-h-0 flex-1 overflow-y-auto p-2">
        {tab === 'runs' && (
          <RunsTab
            workspaceId={workspaceId}
            runs={runsQuery.data?.workflow_runs}
            isLoading={runsQuery.isLoading}
            isError={runsQuery.isError}
            error={runsQuery.error}
            workflows={workflowsQuery.data?.workflows}
            workflowFilter={workflowFilter}
            onFilterChange={setWorkflowFilter}
            invalidateRuns={invalidateRuns}
          />
        )}
        {tab === 'workflows' && (
          <WorkflowsTab
            workspaceId={workspaceId}
            workflows={workflowsQuery.data?.workflows}
            isLoading={workflowsQuery.isLoading}
            isError={workflowsQuery.isError}
            error={workflowsQuery.error}
            defaultRef={repo?.repo_info?.default_branch || 'main'}
            invalidateRuns={invalidateRuns}
          />
        )}
      </div>
    </div>
  );
}

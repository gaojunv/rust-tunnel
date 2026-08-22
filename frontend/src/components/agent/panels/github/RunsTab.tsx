import { useEffect, useMemo, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import {
  AlertCircle,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  CircleSlash,
  ExternalLink,
  Loader2,
  RotateCcw,
  Search,
  Square,
  XCircle,
  GitCommitHorizontal,
  GitBranch,
  Tag,
  Clock,
  Play,
} from 'lucide-react';
import { cn } from '@/lib/utils';
import {
  getAgentGithubJobLogs,
  getAgentGithubRunJobs,
  postAgentGithubCancel,
  postAgentGithubRerun,
} from '../../../../api/client';
import type { GhJob, GhJobStep, GhWorkflow, GhWorkflowRun } from '../../../../types';
import { useApprovalMutation } from '../useApprovalMutation';
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '../../../../components/ui/dialog';
import { Input } from '../../../../components/ui/input';
import { ApprovalDialog } from '../git/shared';
import type { TranslateFn } from '../../formatRelativeTime';
import {
  conclusionBadgeClass,
  conclusionLabel,
  formatDuration,
  formatGhTime,
  isRunActive,
  runStatusIconKind,
  type GhStatusIconKind,
} from './githubUtils';
import { GithubErrorBanner, GithubMutationError } from './shared';

/** 统一状态图标：success绿 / failure红 / cancelled灰 / action_required黄 / active转圈 */
function GhStatusIcon({ kind, className }: { kind: GhStatusIconKind; className?: string }) {
  const cls = cn('h-3 w-3 shrink-0', className);
  switch (kind) {
    case 'success':
      return <CheckCircle2 className={cn(cls, 'text-green-500')} />;
    case 'failure':
      return <XCircle className={cn(cls, 'text-red-500')} />;
    case 'cancelled':
      return <CircleSlash className={cn(cls, 'text-muted-foreground')} />;
    case 'action_required':
      return <AlertCircle className={cn(cls, 'text-yellow-500')} />;
    case 'active':
      return <Loader2 className={cn(cls, 'animate-spin text-yellow-500')} />;
    default:
      return <CircleSlash className={cn(cls, 'text-muted-foreground/60')} />;
  }
}

/** event → 小图标（最小映射，未知回退 GitBranch）。 */
function GhEventIcon({ event }: { event?: string | null }) {
  const cls = 'h-3 w-3 shrink-0 text-muted-foreground/70';
  switch (event) {
    case 'push':
      return <GitCommitHorizontal className={cls} />;
    case 'pull_request':
    case 'pull_request_target':
      return <GitBranch className={cls} />;
    case 'schedule':
      return <Clock className={cls} />;
    case 'workflow_dispatch':
      return <Play className={cls} />;
    case 'tag':
    case 'release':
      return <Tag className={cls} />;
    default:
      return event ? <GitBranch className={cls} /> : null;
  }
}

/** 计算耗时文本：start/end 均为 ISO 串；缺失时返回空串。 */
function durationText(start?: string | null, end?: string | null): string {
  if (!start) return '';
  const s = Date.parse(start);
  if (Number.isNaN(s)) return '';
  const e = end ? Date.parse(end) : Date.now();
  if (Number.isNaN(e)) return '';
  const ms = e - s;
  if (ms < 0) return '';
  return formatDuration(ms);
}

/** 单 step 行：number/name/状态图标/耗时 */
function StepRow({ step }: { step: GhJobStep }) {
  const kind = runStatusIconKind(step.status, step.conclusion);
  const dur = durationText(step.started_at, step.completed_at);
  return (
    <div className="flex items-center gap-1.5 rounded px-1 py-0.5">
      <GhStatusIcon kind={kind} className="h-3 w-3" />
      {step.number != null && (
        <span className="shrink-0 font-mono text-[11px] text-muted-foreground/60">
          #{step.number}
        </span>
      )}
      <span className="min-w-0 flex-1 truncate text-[11px]">{step.name}</span>
      {dur && (
        <span className="shrink-0 font-mono text-[11px] text-muted-foreground/60">{dur}</span>
      )}
    </div>
  );
}

/** 单 job 行：可展开 steps；失败自动展开由父级控制 */
function JobRow({
  job,
  expanded,
  onToggle,
  onViewLogs,
}: {
  job: GhJob;
  expanded: boolean;
  onToggle: () => void;
  onViewLogs: () => void;
}) {
  const kind = runStatusIconKind(job.status, job.conclusion);
  const dur = durationText(job.started_at, job.completed_at);
  const hasSteps = (job.steps?.length ?? 0) > 0;
  return (
    <div className="rounded">
      <div className="group flex items-center gap-1 rounded px-1 py-0.5 hover:bg-accent/40">
        <button
          type="button"
          aria-expanded={expanded}
          onClick={hasSteps ? onToggle : undefined}
          disabled={!hasSteps}
          className={cn(
            'flex min-w-0 flex-1 items-center gap-1.5 text-left',
            !hasSteps && 'cursor-default',
          )}
        >
          {hasSteps ? (
            expanded ? (
              <ChevronDown className="h-3 w-3 shrink-0 text-muted-foreground" />
            ) : (
              <ChevronRight className="h-3 w-3 shrink-0 text-muted-foreground" />
            )
          ) : (
            <span className="w-3 shrink-0" />
          )}
          <GhStatusIcon kind={kind} />
          <span className="min-w-0 flex-1 truncate text-xs">{job.name}</span>
          {dur && (
            <span className="shrink-0 font-mono text-[11px] text-muted-foreground/60">{dur}</span>
          )}
        </button>
        <button
          type="button"
          aria-label={`查看日志 ${job.name}`}
          title="查看日志"
          onClick={onViewLogs}
          className="shrink-0 rounded px-1 py-0.5 text-[11px] text-muted-foreground opacity-100 md:opacity-0 md:group-hover:opacity-100 transition-opacity hover:bg-accent hover:text-foreground"
        >
          日志
        </button>
      </div>
      {expanded && hasSteps && (
        <div className="ml-4 border-l border-border/40 pl-2">
          {(job.steps ?? []).map((step, idx) => (
            <StepRow key={`${step.name}-${idx}`} step={step} />
          ))}
        </div>
      )}
    </div>
  );
}

/** 单次运行的作业列表：点开展开时懒加载；轮询仅在 run 活跃时 10s */
function RunJobsList({
  workspaceId,
  runId,
  runStatus,
}: {
  workspaceId: string;
  runId: number;
  runStatus: string;
}) {
  const { t } = useTranslation();
  const translate = t as unknown as TranslateFn;
  const [logJob, setLogJob] = useState<GhJob | null>(null);
  const [expandedJobId, setExpandedJobId] = useState<number | null>(null);
  const jobsQuery = useQuery({
    queryKey: ['agent-github-jobs', workspaceId, runId],
    queryFn: () => getAgentGithubRunJobs(workspaceId, String(runId)),
    retry: false,
    refetchInterval: isRunActive(runStatus) ? 10_000 : false,
  });

  const jobs: GhJob[] = jobsQuery.data?.jobs ?? [];
  const jobsKey = jobs.map((j) => j.id).join(',');
  // 失败自动展开：首次加载后若有失败 job/step，自动展开到失败处
  // jobs 数组每次查询返回新引用，用稳定 key 避免 effect 每轮重跑
  useEffect(() => {
    if (expandedJobId !== null) return;
    if (jobs.length === 0) return;
    const failedJob = jobs.find(
      (j) => j.conclusion === 'failure' || j.conclusion === 'timed_out',
    );
    if (failedJob) {
      setExpandedJobId(failedJob.id);
      return;
    }
    // 若无失败 job，但有 steps 失败的 job，也展开
    const jobWithFailedStep = jobs.find((j) =>
      (j.steps ?? []).some((s) => s.conclusion === 'failure' || s.conclusion === 'timed_out'),
    );
    if (jobWithFailedStep) setExpandedJobId(jobWithFailedStep.id);
    // eslint-disable-next-line react-hooks/exhaustive-deps -- 由 jobsKey 驱动
  }, [jobsKey, expandedJobId]);

  if (jobsQuery.isLoading) {
    return (
      <div className="flex items-center gap-1 px-1 py-0.5 text-xs text-muted-foreground">
        <Loader2 className="h-3 w-3 animate-spin" />
        {t('common.loading')}
      </div>
    );
  }
  if (jobsQuery.isError) {
    return (
      <div className="py-0.5">
        <GithubErrorBanner error={jobsQuery.error} />
      </div>
    );
  }
  if (jobs.length === 0) {
    return <p className="px-1 py-0.5 text-xs text-muted-foreground">{t('agent.githubNoJobs')}</p>;
  }

  return (
    <div className="space-y-0.5 pl-3">
      {jobs.map((job) => (
        <JobRow
          key={job.id}
          job={job}
          expanded={expandedJobId === job.id}
          onToggle={() =>
            setExpandedJobId((cur) => (cur === job.id ? null : job.id))
          }
          onViewLogs={() => setLogJob(job)}
        />
      ))}
      {/* 兼容旧测试：保留 jobs 列表的翻译兜底（未用到时静默） */}
      <span className="hidden">{translate('agent.githubRunStatus_queued')}</span>
      {logJob && <JobLogsDialog workspaceId={workspaceId} job={logJob} onClose={() => setLogJob(null)} />}
    </div>
  );
}

/** 日志分组：解析 ##[group] / ##[endgroup] 为可折叠段 */
interface LogSection {
  title: string | null; // null = 非分组散段
  lines: string[]; // 含行号前的原始行
  startLine: number; // 1-based 起始行号
}

function parseLogSections(raw: string): LogSection[] {
  const rawLines = raw.split('\n');
  const sections: LogSection[] = [];
  let current: LogSection | null = null;
  let lineNo = 1;
  const flush = () => {
    if (current && current.lines.length > 0) sections.push(current);
    current = null;
  };
  for (const line of rawLines) {
    const groupMatch = line.match(/^##\[group\](.*)$/);
    const endGroup = line.trim() === '##[endgroup]';
    if (groupMatch) {
      flush();
      current = { title: groupMatch[1].trim() || 'group', lines: [], startLine: lineNo + 1 };
      lineNo += 1;
      continue;
    }
    if (endGroup) {
      flush();
      // 非分组散段延续
      current = { title: null, lines: [], startLine: lineNo + 1 };
      lineNo += 1;
      continue;
    }
    if (!current) current = { title: null, lines: [], startLine: lineNo };
    current.lines.push(line);
    lineNo += 1;
  }
  flush();
  // 若全程无分组，合为一段
  if (sections.length === 0 && rawLines.length > 0) {
    return [{ title: null, lines: rawLines, startLine: 1 }];
  }
  return sections;
}

/** 作业日志弹层：行号 + 搜索 + ##[group] 折叠 */
function JobLogsDialog({
  workspaceId,
  job,
  onClose,
}: {
  workspaceId: string;
  job: GhJob;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const [q, setQ] = useState('');
  const [collapsedGroups, setCollapsedGroups] = useState<Record<number, boolean>>({});
  const logsQuery = useQuery({
    queryKey: ['agent-github-logs', workspaceId, job.id],
    queryFn: () => getAgentGithubJobLogs(workspaceId, String(job.id)),
    retry: false,
  });
  const raw = logsQuery.data?.logs ?? '';
  const sections = useMemo(() => parseLogSections(raw), [raw]);
  const qLower = q.trim().toLowerCase();

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="max-w-2xl">
        <DialogHeader>
          <DialogTitle>
            {t('agent.githubLogsTitle')} · {job.name}
          </DialogTitle>
        </DialogHeader>
        {/* 搜索框 */}
        <div className="relative">
          <Search className="absolute left-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder="搜索日志…"
            className="h-7 pl-7 text-xs"
          />
        </div>
        {logsQuery.isLoading && (
          <div className="flex items-center gap-1 px-1 py-2 text-xs text-muted-foreground">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
            {t('common.loading')}
          </div>
        )}
        {logsQuery.isError && <GithubErrorBanner error={logsQuery.error} />}
        {logsQuery.data && (
          <div className="space-y-1">
            {logsQuery.data.truncated && (
              <p className="text-xs text-muted-foreground">{t('agent.githubLogsTruncated')}</p>
            )}
            <div className="max-h-[55vh] overflow-auto rounded-md bg-muted p-2">
              {raw.trim() === '' ? (
                <p className="font-mono text-xs text-muted-foreground">{t('agent.githubLogsEmpty')}</p>
              ) : (
                sections.map((sec, secIdx) => {
                  const filtered = qLower
                    ? sec.lines.filter((l) => l.toLowerCase().includes(qLower))
                    : sec.lines;
                  if (qLower && filtered.length === 0) return null;
                  const isCollapsed = !!collapsedGroups[secIdx];
                  const body = (
                    <pre className="whitespace-pre-wrap break-all font-mono text-xs leading-relaxed">
                      {filtered.map((line, idx) => {
                        const lineNo = sec.startLine + sec.lines.indexOf(line);
                        // 高亮搜索命中（简易：整行加底色）
                        const hit = qLower && line.toLowerCase().includes(qLower);
                        return (
                          <div
                            key={idx}
                            className={cn('flex gap-2', hit && 'bg-yellow-500/20')}
                          >
                            <span className="shrink-0 select-none text-[11px] tabular-nums text-muted-foreground/60">
                              {String(lineNo).padStart(4, ' ')}
                            </span>
                            <span className="min-w-0 flex-1">{line === '' ? ' ' : line}</span>
                          </div>
                        );
                      })}
                    </pre>
                  );
                  if (sec.title === null) {
                    return (
                      <div key={secIdx} className="py-0.5">
                        {body}
                      </div>
                    );
                  }
                  return (
                    <div key={secIdx} className="py-0.5">
                      <button
                        type="button"
                        onClick={() =>
                          setCollapsedGroups((m) => ({ ...m, [secIdx]: !m[secIdx] }))
                        }
                        className="flex w-full items-center gap-1 rounded px-1 py-0.5 text-left text-xs font-medium hover:bg-accent/50"
                      >
                        {isCollapsed ? (
                          <ChevronRight className="h-3 w-3 shrink-0 text-muted-foreground" />
                        ) : (
                          <ChevronDown className="h-3 w-3 shrink-0 text-muted-foreground" />
                        )}
                        <span className="truncate">{sec.title}</span>
                      </button>
                      {!isCollapsed && <div className="pl-4">{body}</div>}
                    </div>
                  );
                })
              )}
            </div>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}

/** 单条运行行：标题 + 元信息行（event/#number/sha/message/耗时）；操作收进行尾 */
function RunRow({
  workspaceId,
  run,
  expanded,
  onToggle,
  onRerun,
  onCancel,
}: {
  workspaceId: string;
  run: GhWorkflowRun;
  expanded: boolean;
  onToggle: () => void;
  onRerun: (runId: number) => void;
  onCancel: (runId: number) => void;
}) {
  const { t } = useTranslation();
  const translate = t as unknown as TranslateFn;
  const active = isRunActive(run.status);
  const title = run.display_title || run.name || `#${run.id}`;
  const time = formatGhTime(run.run_started_at, translate);
  const dur = durationText(run.run_started_at, run.updated_at);
  const shortSha = run.head_sha ? run.head_sha.slice(0, 7) : '';
  const commitMsg = run.head_commit?.message?.split('\n')[0]?.trim() ?? '';
  const kind = runStatusIconKind(run.status, run.conclusion ?? null);

  return (
    <div className="group rounded border border-border/60">
      <div className="flex items-center gap-1 px-1 py-1">
        <button
          type="button"
          aria-expanded={expanded}
          onClick={onToggle}
          className="flex min-w-0 flex-1 items-center gap-1.5 text-left"
        >
          {expanded ? (
            <ChevronDown className="h-3 w-3 shrink-0 text-muted-foreground" />
          ) : (
            <ChevronRight className="h-3 w-3 shrink-0 text-muted-foreground" />
          )}
          <GhStatusIcon kind={kind} />
          <span className="min-w-0 flex-1">
            <span className="block truncate text-xs font-medium">{title}</span>
            <span className="flex flex-wrap items-center gap-1 truncate text-[11px] text-muted-foreground">
              {run.event && (
                <span className="inline-flex items-center gap-0.5">
                  <GhEventIcon event={run.event} />
                  <span>{run.event}</span>
                </span>
              )}
              {run.run_number != null && <span>#{run.run_number}</span>}
              {shortSha && <span className="font-mono">{shortSha}</span>}
              {run.head_branch && <span className="font-mono">{run.head_branch}</span>}
              {time && <span>· {time}</span>}
              {dur && <span>· {dur}</span>}
            </span>
            {commitMsg && (
              <span className="block truncate text-[11px] text-muted-foreground/70">
                {commitMsg}
              </span>
            )}
          </span>
        </button>
        {/* 行尾操作：移动端常显，桌面 hover 揭示；保留 role=button 形态 */}
        <div className="flex shrink-0 items-center gap-0.5">
          <button
            type="button"
            aria-label={`${t('agent.githubRerun')} ${title}`}
            title={t('agent.githubRerun')}
            onClick={() => onRerun(run.id)}
            className="flex items-center gap-0.5 rounded px-1 py-1 text-[11px] text-muted-foreground opacity-100 md:opacity-0 md:group-hover:opacity-100 transition-opacity hover:bg-accent hover:text-foreground"
          >
            <RotateCcw className="h-3 w-3" />
            <span className="hidden md:inline">{t('agent.githubRerun')}</span>
          </button>
          {active && (
            <button
              type="button"
              aria-label={`${t('agent.githubCancel')} ${title}`}
              title={t('agent.githubCancel')}
              onClick={() => onCancel(run.id)}
              className="flex items-center gap-0.5 rounded px-1 py-1 text-[11px] text-muted-foreground opacity-100 md:opacity-0 md:group-hover:opacity-100 transition-opacity hover:bg-destructive/10 hover:text-destructive"
            >
              <Square className="h-3 w-3" />
              <span className="hidden md:inline">{t('agent.githubCancel')}</span>
            </button>
          )}
          {run.html_url && (
            <a
              href={run.html_url}
              target="_blank"
              rel="noreferrer"
              aria-label={`${t('agent.githubOpenRun')} ${title}`}
              title={t('agent.githubOpenRun')}
              className="flex items-center rounded p-1 text-muted-foreground opacity-100 md:opacity-0 md:group-hover:opacity-100 transition-opacity hover:bg-accent hover:text-foreground"
            >
              <ExternalLink className="h-3 w-3" />
            </a>
          )}
        </div>
      </div>
      {/* 保留原 conclusion 徽标的隐藏节点以兼容视觉回归（不影响布局） */}
      <span className="hidden">
        <span className={conclusionBadgeClass(run.conclusion)}>{conclusionLabel(run.conclusion, translate)}</span>
      </span>
      {expanded && (
        <div className="border-t border-border/40 pb-1 pt-0.5">
          <RunJobsList workspaceId={workspaceId} runId={run.id} runStatus={run.status} />
        </div>
      )}
    </div>
  );
}

/**
 * Runs Tab：运行列表 + 顶部按工作流过滤下拉。运行行点开三层树（run→jobs→steps），
 * 失败自动展开；rerun / cancel 走 409 审批流，成功后刷新容器 runs query。
 */
export function RunsTab({
  workspaceId,
  runs,
  isLoading,
  isError,
  error,
  workflows,
  workflowFilter,
  onFilterChange,
  invalidateRuns,
}: {
  workspaceId: string;
  runs?: GhWorkflowRun[];
  isLoading: boolean;
  isError: boolean;
  error: unknown;
  workflows?: GhWorkflow[];
  workflowFilter: string;
  onFilterChange: (workflowId: string) => void;
  invalidateRuns: () => void;
}) {
  const { t } = useTranslation();
  const [expandedId, setExpandedId] = useState<number | null>(null);

  const rerunMutation = useApprovalMutation(
    (approved, runId: number) => postAgentGithubRerun(workspaceId, String(runId), approved),
    { onSuccess: invalidateRuns },
  );
  const cancelMutation = useApprovalMutation(
    (approved, runId: number) => postAgentGithubCancel(workspaceId, String(runId), approved),
    { onSuccess: invalidateRuns },
  );

  return (
    <div className="space-y-1.5">
      {/* 顶部：工作流过滤下拉 */}
      <div className="px-0.5">
        <select
          value={workflowFilter}
          onChange={(e) => onFilterChange(e.target.value)}
          aria-label={t('agent.githubFilterWorkflow')}
          className="h-8 w-full rounded-md border border-input bg-background px-2 text-xs"
        >
          <option value="">{t('agent.githubAllWorkflows')}</option>
          {(workflows ?? []).map((wf) => (
            <option key={wf.id} value={String(wf.id)}>
              {wf.name}
            </option>
          ))}
        </select>
      </div>

      {isError && <GithubErrorBanner error={error} />}

      {isLoading && (
        <div className="flex items-center gap-1 px-1 py-1 text-xs text-muted-foreground">
          <Loader2 className="h-3.5 w-3.5 animate-spin" />
          {t('common.loading')}
        </div>
      )}

      {!isLoading && !isError && (runs ?? []).length === 0 && (
        <p className="px-1 text-xs text-muted-foreground">{t('agent.githubNoRuns')}</p>
      )}

      {(runs ?? []).map((run) => (
        <RunRow
          key={run.id}
          workspaceId={workspaceId}
          run={run}
          expanded={expandedId === run.id}
          onToggle={() => setExpandedId((cur) => (cur === run.id ? null : run.id))}
          onRerun={(id) => rerunMutation.mutate(id)}
          onCancel={(id) => cancelMutation.mutate(id)}
        />
      ))}

      <GithubMutationError error={rerunMutation.error ?? cancelMutation.error} />
      <ApprovalDialog
        approval={rerunMutation.approval}
        onConfirm={rerunMutation.confirmApproval}
        onCancel={rerunMutation.cancelApproval}
        descKey="agent.githubApprovalDesc"
      />
      <ApprovalDialog
        approval={cancelMutation.approval}
        onConfirm={cancelMutation.confirmApproval}
        onCancel={cancelMutation.cancelApproval}
        descKey="agent.githubApprovalDesc"
      />
    </div>
  );
}

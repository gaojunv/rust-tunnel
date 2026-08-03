import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { StatCard } from '@/components/shared/StatCard';
import { TimeRangeSelector } from '@/components/shared/TimeRangeSelector';
import { useTimeRange } from '@/hooks/useTimeRange';
import { useLlmUsageSummary, useLlmUsageAggregate, useLlmUsageLogs } from '@/api/hooks';
import type { UsageGroupBy } from '@/types';
import { Activity, Coins, CheckCircle2, Database, GitCompareArrows, ChevronLeft, ChevronRight } from 'lucide-react';

const GROUP_BY_LABEL_KEYS = {
  api_key: 'llm.usage.groupBy.api_key',
  model: 'llm.usage.groupBy.model',
  provider: 'llm.usage.groupBy.provider',
} as const;

const fmt = (n: number): string => n.toLocaleString('en-US');

const pct = (num: number, denom: number): string =>
  denom === 0 ? '—' : `${((num / denom) * 100).toFixed(1)}%`;

const PAGE_SIZE = 20;

export default function UsageTab() {
  const { t } = useTranslation();
  const { range, preset, presets, setPreset, setCustomRange } = useTimeRange('24h');
  const [groupBy, setGroupBy] = useState<UsageGroupBy>('api_key');
  const [page, setPage] = useState(0);

  // TimeRangeSelector 用 ms；查询接口用 RFC3339 字符串
  const apiRange = useMemo(
    () => ({
      start: new Date(range.startMs).toISOString(),
      end: new Date(range.endMs).toISOString(),
    }),
    [range.startMs, range.endMs]
  );

  // 时间范围或分组变化时重置页码
  useEffect(() => {
    setPage(0);
  }, [apiRange.start, apiRange.end, groupBy]);

  const { data: summary } = useLlmUsageSummary(apiRange);
  const { data: rows, isLoading: rowsLoading } = useLlmUsageAggregate(groupBy, apiRange);
  const { data: logsData, isLoading: logsLoading } = useLlmUsageLogs(
    apiRange,
    PAGE_SIZE,
    page * PAGE_SIZE
  );

  const logs = logsData?.logs ?? [];
  const totalLogs = logsData?.total ?? 0;
  const totalPages = Math.ceil(totalLogs / PAGE_SIZE);
  const hasNextPage = page < totalPages - 1;
  const hasPrevPage = page > 0;

  const successRate = summary ? pct(summary.success, summary.requests) : '—';
  const cacheRate = summary ? pct(summary.cache_hit_tokens, summary.prompt_tokens) : '—';

  // 转移率：当前页日志中 failover_from 非空的比例（前端计算）
  const failoverRate = useMemo(() => {
    const pageLogs = logsData?.logs ?? [];
    if (pageLogs.length === 0) return 0;
    return pageLogs.filter((l) => l.failover_from).length / pageLogs.length;
  }, [logsData]);

  return (
    <div className="space-y-6">
      {/* 时间范围 */}
      <div className="flex justify-end">
        <TimeRangeSelector
          preset={preset}
          presets={presets}
          customStartMs={range.startMs}
          customEndMs={range.endMs}
          onPresetChange={setPreset}
          onCustomChange={setCustomRange}
        />
      </div>

      {/* Stat Cards */}
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-5">
        <StatCard
          title={t('llm.usage.stats.totalRequests')}
          value={summary ? fmt(summary.requests) : '—'}
          icon={<Activity className="h-4 w-4" />}
        />
        <StatCard
          title={t('llm.usage.stats.totalTokens')}
          value={summary ? fmt(summary.total_tokens) : '—'}
          description={
            summary
              ? t('llm.usage.stats.promptDesc', { prompt: fmt(summary.prompt_tokens), completion: fmt(summary.completion_tokens) })
              : undefined
          }
          icon={<Coins className="h-4 w-4" />}
        />
        <StatCard
          title={t('llm.usage.stats.successRate')}
          value={successRate}
          description={summary ? t('llm.usage.stats.successDesc', { success: fmt(summary.success), total: fmt(summary.requests) }) : undefined}
          icon={<CheckCircle2 className="h-4 w-4" />}
        />
        <StatCard
          title={t('llm.usage.stats.cacheHitRate')}
          value={cacheRate}
          description={summary ? t('llm.usage.stats.cacheDesc', { tokens: fmt(summary.cache_hit_tokens) }) : undefined}
          icon={<Database className="h-4 w-4" />}
        />
        <StatCard
          title={t('llm.usage.failoverRate')}
          value={`${(failoverRate * 100).toFixed(1)}%`}
          description={t('llm.usage.failoverRateDesc', { count: logs.filter((l) => l.failover_from).length, total: logs.length })}
          icon={<GitCompareArrows className="h-4 w-4" />}
        />
      </div>

      {/* Dimension Aggregate */}
      <Card>
        <CardHeader className="flex flex-col gap-3 space-y-0 sm:flex-row sm:items-center sm:justify-between">
          <CardTitle>{t('llm.usage.table.title')}</CardTitle>
          <Select value={groupBy} onValueChange={(v) => setGroupBy(v as UsageGroupBy)}>
            <SelectTrigger className="w-40">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {(Object.keys(GROUP_BY_LABEL_KEYS) as UsageGroupBy[]).map((g) => (
                <SelectItem key={g} value={g}>
                  {t('llm.usage.groupBy.item', { label: t(GROUP_BY_LABEL_KEYS[g]) })}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </CardHeader>
        <CardContent>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>{t(GROUP_BY_LABEL_KEYS[groupBy])}</TableHead>
                <TableHead className="text-right">{t('llm.usage.table.requests')}</TableHead>
                <TableHead className="text-right">{t('llm.usage.table.successRate')}</TableHead>
                <TableHead className="text-right">{t('llm.usage.table.prompt')}</TableHead>
                <TableHead className="text-right">{t('llm.usage.table.cacheHit')}</TableHead>
                <TableHead className="text-right">{t('llm.usage.table.completion')}</TableHead>
                <TableHead className="text-right">{t('llm.usage.table.total')}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {rowsLoading ? (
                <TableRow>
                  <TableCell colSpan={7} className="text-center text-muted-foreground">
                    {t('llm.usage.loading')}
                  </TableCell>
                </TableRow>
              ) : !rows || rows.length === 0 ? (
                <TableRow>
                  <TableCell colSpan={7} className="text-center text-muted-foreground">
                    {t('llm.usage.empty')}
                  </TableCell>
                </TableRow>
              ) : (
                rows.map((r) => (
                  <TableRow key={r.dimension_id ?? '__none__'}>
                    <TableCell className="font-medium">
                      {r.dimension_name || t('llm.usage.unknown')}
                    </TableCell>
                    <TableCell className="text-right tabular-nums">{fmt(r.requests)}</TableCell>
                    <TableCell className="text-right tabular-nums">
                      {pct(r.success, r.requests)}
                    </TableCell>
                    <TableCell className="text-right tabular-nums">{fmt(r.prompt_tokens)}</TableCell>
                    <TableCell className="text-right tabular-nums">
                      {fmt(r.cache_hit_tokens)}
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {fmt(r.completion_tokens)}
                    </TableCell>
                    <TableCell className="text-right tabular-nums font-medium">
                      {fmt(r.total_tokens)}
                    </TableCell>
                  </TableRow>
                ))
              )}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      {/* 明细请求日志 */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <CardTitle>{t('llm.usage.detail.title')}</CardTitle>
          {totalLogs > 0 && (
            <span className="text-sm text-muted-foreground">
              {t('llm.usage.detail.total', { count: fmt(totalLogs) })}
            </span>
          )}
        </CardHeader>
        <CardContent>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>{t('llm.usage.detail.time')}</TableHead>
                <TableHead>{t('llm.usage.detail.apiKey')}</TableHead>
                <TableHead>{t('llm.usage.detail.provider')}</TableHead>
                <TableHead>{t('llm.usage.detail.model')}</TableHead>
                <TableHead>{t('llm.usage.failover')}</TableHead>
                <TableHead>{t('llm.usage.detail.protocol')}</TableHead>
                <TableHead className="text-right">{t('llm.usage.detail.io')}</TableHead>
                <TableHead className="text-right">{t('llm.usage.detail.status')}</TableHead>
                <TableHead className="text-right">{t('llm.usage.detail.latency')}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {logsLoading ? (
                <TableRow>
                  <TableCell colSpan={9} className="text-center text-muted-foreground">
                    {t('llm.usage.loading')}
                  </TableCell>
                </TableRow>
              ) : logs.length === 0 ? (
                <TableRow>
                  <TableCell colSpan={9} className="text-center text-muted-foreground">
                    {t('llm.usage.detail.empty')}
                  </TableCell>
                </TableRow>
              ) : (
                logs.map((l) => (
                  <TableRow key={l.id}>
                    <TableCell className="whitespace-nowrap text-xs text-muted-foreground">
                      {new Date(l.timestamp).toLocaleString()}
                    </TableCell>
                    <TableCell>{l.api_key_name || '—'}</TableCell>
                    <TableCell>{l.provider_name || '—'}</TableCell>
                    <TableCell className="font-mono text-xs">
                      {l.model_name || l.requested_model}
                    </TableCell>
                    <TableCell>
                      {l.failover_from ? (
                        <Badge variant="outline" title={l.failover_from}>
                          {l.failover_from} → {l.model_name || l.requested_model}
                        </Badge>
                      ) : (
                        <span className="text-muted-foreground">-</span>
                      )}
                    </TableCell>
                    <TableCell>
                      {l.protocol}
                      {l.stream ? ` ${t('llm.usage.table.stream')}` : ''}
                    </TableCell>
                    <TableCell className="text-right tabular-nums text-xs">
                      {fmt(l.prompt_tokens)} / {fmt(l.cache_hit_tokens)} / {fmt(l.completion_tokens)}
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      <span className={l.success ? 'text-emerald-500' : 'text-red-500'}>
                        {l.status_code}
                      </span>
                    </TableCell>
                    <TableCell className="text-right tabular-nums text-xs">
                      {fmt(l.latency_ms)}ms
                    </TableCell>
                  </TableRow>
                ))
              )}
            </TableBody>
          </Table>

          {/* Pagination */}
          {totalPages > 1 && (
            <div className="mt-4 flex items-center justify-between">
              <div className="text-sm text-muted-foreground">
                {t('llm.usage.pagination.page', { current: page + 1, total: totalPages })}
              </div>
              <div className="flex items-center gap-2">
                <Button
                  variant="outline"
                  size="sm"
                  disabled={!hasPrevPage}
                  onClick={() => setPage((p) => Math.max(0, p - 1))}
                >
                  <ChevronLeft className="h-4 w-4" />
                  {t('llm.usage.pagination.prev')}
                </Button>
                <Button
                  variant="outline"
                  size="sm"
                  disabled={!hasNextPage}
                  onClick={() => setPage((p) => p + 1)}
                >
                  {t('llm.usage.pagination.next')}
                  <ChevronRight className="h-4 w-4" />
                </Button>
              </div>
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}

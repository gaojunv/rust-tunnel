import { useMemo, useState } from 'react';
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
import { StatCard } from '@/components/shared/StatCard';
import { TimeRangeSelector } from '@/components/shared/TimeRangeSelector';
import { useTimeRange } from '@/hooks/useTimeRange';
import { useLlmUsageSummary, useLlmUsageAggregate, useLlmUsageLogs } from '@/api/hooks';
import type { UsageGroupBy } from '@/types';
import { Activity, Coins, CheckCircle2, Database } from 'lucide-react';

const GROUP_LABELS: Record<UsageGroupBy, string> = {
  api_key: 'API Key',
  model: '模型',
  provider: '供应商',
};

const fmt = (n: number): string => n.toLocaleString('en-US');

const pct = (num: number, denom: number): string =>
  denom === 0 ? '—' : `${((num / denom) * 100).toFixed(1)}%`;

export default function UsageTab() {
  const { range, preset, presets, setPreset, setCustomRange } = useTimeRange('24h');
  const [groupBy, setGroupBy] = useState<UsageGroupBy>('api_key');

  // TimeRangeSelector 用 ms；查询接口用 RFC3339 字符串
  const apiRange = useMemo(
    () => ({
      start: new Date(range.startMs).toISOString(),
      end: new Date(range.endMs).toISOString(),
    }),
    [range.startMs, range.endMs]
  );

  const { data: summary } = useLlmUsageSummary(apiRange);
  const { data: rows, isLoading: rowsLoading } = useLlmUsageAggregate(groupBy, apiRange);
  const { data: logs, isLoading: logsLoading } = useLlmUsageLogs(apiRange, 50, 0);

  const successRate = summary ? pct(summary.success, summary.requests) : '—';
  const cacheRate = summary ? pct(summary.cache_hit_tokens, summary.prompt_tokens) : '—';

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

      {/* 总览卡片 */}
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-4">
        <StatCard
          title="总请求数"
          value={summary ? fmt(summary.requests) : '—'}
          icon={<Activity className="h-4 w-4" />}
        />
        <StatCard
          title="总 Tokens"
          value={summary ? fmt(summary.total_tokens) : '—'}
          description={
            summary
              ? `输入 ${fmt(summary.prompt_tokens)} · 输出 ${fmt(summary.completion_tokens)}`
              : undefined
          }
          icon={<Coins className="h-4 w-4" />}
        />
        <StatCard
          title="成功率"
          value={successRate}
          description={summary ? `${fmt(summary.success)}/${fmt(summary.requests)}` : undefined}
          icon={<CheckCircle2 className="h-4 w-4" />}
        />
        <StatCard
          title="缓存命中率"
          value={cacheRate}
          description={summary ? `命中 ${fmt(summary.cache_hit_tokens)} tokens` : undefined}
          icon={<Database className="h-4 w-4" />}
        />
      </div>

      {/* 维度聚合 */}
      <Card>
        <CardHeader className="flex flex-col gap-3 space-y-0 sm:flex-row sm:items-center sm:justify-between">
          <CardTitle>用量统计</CardTitle>
          <Select value={groupBy} onValueChange={(v) => setGroupBy(v as UsageGroupBy)}>
            <SelectTrigger className="w-40">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {(Object.keys(GROUP_LABELS) as UsageGroupBy[]).map((g) => (
                <SelectItem key={g} value={g}>
                  按{GROUP_LABELS[g]}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </CardHeader>
        <CardContent>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>{GROUP_LABELS[groupBy]}</TableHead>
                <TableHead className="text-right">请求数</TableHead>
                <TableHead className="text-right">成功率</TableHead>
                <TableHead className="text-right">输入</TableHead>
                <TableHead className="text-right">缓存命中</TableHead>
                <TableHead className="text-right">输出</TableHead>
                <TableHead className="text-right">总计</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {rowsLoading ? (
                <TableRow>
                  <TableCell colSpan={7} className="text-center text-muted-foreground">
                    加载中...
                  </TableCell>
                </TableRow>
              ) : !rows || rows.length === 0 ? (
                <TableRow>
                  <TableCell colSpan={7} className="text-center text-muted-foreground">
                    该时间范围内暂无用量数据
                  </TableCell>
                </TableRow>
              ) : (
                rows.map((r) => (
                  <TableRow key={r.dimension_id ?? '__none__'}>
                    <TableCell className="font-medium">
                      {r.dimension_name || '(未知)'}
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
        <CardHeader>
          <CardTitle>请求明细</CardTitle>
        </CardHeader>
        <CardContent>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>时间</TableHead>
                <TableHead>API Key</TableHead>
                <TableHead>供应商</TableHead>
                <TableHead>模型</TableHead>
                <TableHead>协议</TableHead>
                <TableHead className="text-right">输入/命中/输出</TableHead>
                <TableHead className="text-right">状态</TableHead>
                <TableHead className="text-right">耗时</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {logsLoading ? (
                <TableRow>
                  <TableCell colSpan={8} className="text-center text-muted-foreground">
                    加载中...
                  </TableCell>
                </TableRow>
              ) : !logs || logs.length === 0 ? (
                <TableRow>
                  <TableCell colSpan={8} className="text-center text-muted-foreground">
                    该时间范围内暂无请求
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
                      {l.protocol}
                      {l.stream ? ' (stream)' : ''}
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
        </CardContent>
      </Card>
    </div>
  );
}

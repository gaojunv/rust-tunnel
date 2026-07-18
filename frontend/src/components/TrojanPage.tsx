import React, { useMemo } from 'react';
import { useQuery } from '@tanstack/react-query';
import { getTrojanConfig, getTrojanStats, getTrojanQuality } from '../api/client';
import type { TrojanQuality } from '../types';
import { getQualityColor, getQualityText } from './ClientList';
import { LineChart, Line, XAxis, YAxis, CartesianGrid } from 'recharts';
import { formatBytes, formatBps } from '../utils/format';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import {
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
  ChartLegend,
  ChartLegendContent,
  type ChartConfig,
} from '@/components/ui/chart';
import { TimeRangeSelector } from './shared/TimeRangeSelector';
import { useTimeRange, type TimeRange } from '../hooks/useTimeRange';

const ThroughputHistory = ({ qualityList, timeRange }: {
  qualityList: TrojanQuality[];
  timeRange: TimeRange;
}) => {
  const chartConfig = useMemo<ChartConfig>(() => {
    const config: ChartConfig = {};
    qualityList.forEach((q, idx) => {
      config[`in_${q.port}`] = {
        label: `In (Port ${q.port}) B/s`,
        color: `hsl(var(--chart-${((idx * 2) % 5) + 1}))`,
      };
      config[`out_${q.port}`] = {
        label: `Out (Port ${q.port}) B/s`,
        color: `hsl(var(--chart-${((idx * 2 + 1) % 5) + 1}))`,
      };
    });
    return config;
  }, [qualityList]);

  // Merge samples by timestamp (milliseconds) to avoid the old string-key dedup bug
  const timeMap = new Map<number, Record<string, number | string>>();
  for (const q of qualityList) {
    for (const s of q.history) {
      const ts = new Date(s.timestamp).getTime();
      if (ts < timeRange.startMs || ts > timeRange.endMs) continue;
      if (!timeMap.has(ts)) timeMap.set(ts, { time: ts });
      const pt = timeMap.get(ts)!;
      pt[`in_${q.port}`] = s.bytes_in_per_sec;
      pt[`out_${q.port}`] = s.bytes_out_per_sec;
    }
  }
  const chartData = Array.from(timeMap.values())
    .sort((a, b) => (a.time as number) - (b.time as number));

  if (chartData.length === 0) {
    return <p className="py-4 text-center text-sm text-muted-foreground">No throughput data available yet</p>;
  }
  return (
    <ChartContainer config={chartConfig} className="h-[200px] w-full">
      <LineChart data={chartData} margin={{ left: 12, right: 12 }}>
        <CartesianGrid strokeDasharray="3 3" vertical={false} />
        <XAxis
          dataKey="time"
          tickLine={false}
          axisLine={false}
          tickMargin={8}
          tickFormatter={(ts: number) => new Date(ts).toLocaleTimeString()}
        />
        <YAxis
          tickLine={false}
          axisLine={false}
          tickMargin={8}
          tickFormatter={formatBps}
          width={70}
        />
        <ChartTooltip
          content={
            <ChartTooltipContent
              labelFormatter={(ts) => new Date(Number(ts)).toLocaleString()}
              formatter={(value, name) => (
                <div className="flex w-full items-center gap-2">
                  <span
                    className="h-2.5 w-2.5 shrink-0 rounded-[2px]"
                    style={{ backgroundColor: chartConfig[name]?.color }}
                  />
                  <span className="flex-1 text-muted-foreground">
                    {chartConfig[name]?.label ?? name}
                  </span>
                  <span className="font-mono font-medium tabular-nums text-foreground">
                    {formatBps(Number(value))}
                  </span>
                </div>
              )}
            />
          }
        />
        <ChartLegend content={<ChartLegendContent />} />
        {qualityList.map(q => (
          <React.Fragment key={q.port}>
            <Line type="monotone" dataKey={`in_${q.port}`}
              stroke={`var(--color-in_${q.port})`} dot={false} strokeWidth={2} />
            <Line type="monotone" dataKey={`out_${q.port}`}
              stroke={`var(--color-out_${q.port})`} dot={false} strokeWidth={2} />
          </React.Fragment>
        ))}
      </LineChart>
    </ChartContainer>
  );
};

export const TrojanPage = () => {
  const { range, preset, presets, setPreset, setCustomRange } = useTimeRange();

  const { data: config, isLoading: configLoading } = useQuery({
    queryKey: ['trojan-config'],
    queryFn: getTrojanConfig,
    refetchInterval: 5000,
  });

  const { data: stats, isLoading: statsLoading } = useQuery({
    queryKey: ['trojan-stats'],
    queryFn: getTrojanStats,
    refetchInterval: 5000,
  });

  const { data: qualityList = [] } = useQuery<TrojanQuality[]>({
    queryKey: ['trojan-quality'],
    queryFn: getTrojanQuality,
    refetchInterval: 5000,
  });

  if (configLoading || statsLoading) {
    return (
      <div className="flex justify-center items py-8">
        <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-blue-600"></div>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      {/* Configuration Card */}
      <div className="bg-white dark:bg-slate-800 shadow dark:shadow-slate-950/20 rounded-lg p-6">
        <h2 className="text-lg font-semibold text-gray-900 dark:text-slate-100 mb-4">Trojan Configuration</h2>
        <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
          <div>
            <label className="block text-sm font-medium text-gray-500 dark:text-slate-400">Status</label>
            <div className="flex items-center mt-1">
              <span className={`w-3 h-3 rounded-full mr-2 ${config?.enabled ? 'bg-green-500' : 'bg-gray-300'}`}></span>
              <span className="text-lg font-semibold text-gray-900 dark:text-slate-100">
                {config?.enabled ? 'Enabled' : 'Disabled'}
              </span>
            </div>
          </div>
          <div>
            <label className="block text-sm font-medium text-gray-500 dark:text-slate-400">Port</label>
            <p className="text-lg font-semibold text-gray-900 dark:text-slate-100">{config?.port || 'N/A'}</p>
          </div>
          <div>
            <label className="block text-sm font-medium text-gray-500 dark:text-slate-400">Fallback</label>
            <p className="text-lg font-semibold text-gray-900 dark:text-slate-100">{config?.fallback || 'N/A'}</p>
          </div>
        </div>
      </div>

      {/* Statistics Card */}
      <div className="bg-white dark:bg-slate-800 shadow dark:shadow-slate-950/20 rounded-lg p-6">
        <h2 className="text-lg font-semibold text-gray-900 dark:text-slate-100 mb-4">Traffic Statistics</h2>
        <div className="grid grid-cols-1 md:grid-cols-4 gap-4">
          <div className="bg-purple-50 dark:bg-purple-900/30 rounded-lg p-4">
            <label className="block text-sm font-medium text-gray-500 dark:text-slate-400">Enabled</label>
            <p className="text-lg font-semibold text-gray-900 dark:text-slate-100">
              {stats?.enabled ? 'Yes' : 'No'}
            </p>
          </div>
          <div className="bg-blue-50 dark:bg-blue-900/30 rounded-lg p-4">
            <label className="block text-sm font-medium text-gray-500 dark:text-slate-400">Port</label>
            <p className="text-lg font-semibold text-gray-900 dark:text-slate-100">{stats?.port || 'N/A'}</p>
          </div>
          <div className="bg-green-50 dark:bg-green-900/30 rounded-lg p-4">
            <label className="block text-sm font-medium text-gray-500 dark:text-slate-400">Total Bytes In</label>
            <p className="text-lg font-semibold text-gray-900 dark:text-slate-100">
              {formatBytes(stats?.total_bytes_in || 0)}
            </p>
          </div>
          <div className="bg-orange-50 dark:bg-orange-900/30 rounded-lg p-4">
            <label className="block text-sm font-medium text-gray-500 dark:text-slate-400">Total Bytes Out</label>
            <p className="text-lg font-semibold text-gray-900 dark:text-slate-100">
              {formatBytes(stats?.total_bytes_out || 0)}
            </p>
          </div>
        </div>
        {stats && stats.active_connections !== undefined && (
          <div className="mt-4">
            <div className="bg-yellow-50 dark:bg-yellow-900/30 rounded-lg p-4">
              <label className="block text-sm font-medium text-gray-500 dark:text-slate-400">Active Connections</label>
              <p className="text-lg font-semibold text-gray-900 dark:text-slate-100">{stats.active_connections}</p>
            </div>
          </div>
        )}
      </div>

      {/* Quality History */}
      {qualityList.length > 0 && (
        <div className="bg-white dark:bg-slate-800 shadow dark:shadow-slate-950/20 rounded-lg p-6">
          <h2 className="text-lg font-semibold text-gray-900 dark:text-slate-100 mb-4">Quality & Throughput</h2>
          <div className="grid grid-cols-1 md:grid-cols-2 gap-4 mb-4">
            {qualityList.map(q => {
              const color = getQualityColor(q.quality.quality_score);
              return (
                <div key={q.port} className="bg-gray-50 dark:bg-slate-700/50 rounded-lg p-4">
                  <div className="flex items-center justify-between mb-2">
                    <span className="text-sm font-medium text-gray-700 dark:text-slate-200">Port {q.port}</span>
                    <span className="font-semibold" style={{ color }}>
                      Score: {q.quality.quality_score} ({getQualityText(q.quality.quality_score)})
                    </span>
                  </div>
                  <div className="grid grid-cols-2 gap-2 text-xs text-gray-600 dark:text-slate-300">
                    <span>In: {formatBps(q.quality.bytes_in_per_sec)}</span>
                    <span>Out: {formatBps(q.quality.bytes_out_per_sec)}</span>
                  </div>
                </div>
              );
            })}
          </div>
          <div className="flex items-center justify-end mb-4">
            <TimeRangeSelector
              preset={preset}
              presets={presets}
              customStartMs={range.startMs}
              customEndMs={range.endMs}
              onPresetChange={setPreset}
              onCustomChange={setCustomRange}
            />
          </div>
          <Card>
            <CardHeader>
              <CardTitle className="text-lg">Throughput History</CardTitle>
            </CardHeader>
            <CardContent>
              <ThroughputHistory qualityList={qualityList} timeRange={range} />
            </CardContent>
          </Card>
        </div>
      )}
    </div>
  );
};

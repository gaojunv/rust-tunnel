import React, { useState, useCallback } from 'react';
import { useQuery } from '@tanstack/react-query';
import { getTrojanConfig, getTrojanStats, getTrojanQuality } from '../api/client';
import type { TrojanQuality } from '../types';
import { getQualityColor, getQualityText } from './ClientList';
import { LineChart, Line, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer } from 'recharts';
import { formatBytes, formatBps } from '../utils/format';
import { ChartContainer } from './shared/ChartContainer';
import type { ChartTimeRange } from './shared/ChartContainer';
import { useTheme } from '../theme/ThemeProvider';

const ThroughputHistory = ({ qualityList, timeRange }: {
  qualityList: TrojanQuality[];
  timeRange: ChartTimeRange;
}) => {
  const { resolvedTheme } = useTheme();
  const isDark = resolvedTheme === 'dark';
  const axisColor = isDark ? '#94a3b8' : '#6b7280';
  const gridColor = isDark ? '#334155' : '#e5e7eb';
  const tooltipStyle = isDark
    ? { backgroundColor: '#1e293b', border: '1px solid #475569', color: '#f1f5f9' }
    : { backgroundColor: '#ffffff', border: '1px solid #e5e7eb', color: '#111827' };
  const tooltipTextStyle = { color: tooltipStyle.color };

  // Merge samples by timestamp (milliseconds) to avoid the old string-key dedup bug
  const timeMap = new Map<number, Record<string, number | string>>();
  for (const q of qualityList) {
    for (const s of q.history) {
      const ts = new Date(s.timestamp).getTime();
      if (ts < timeRange.startMs || ts > timeRange.endMs) continue;
      if (!timeMap.has(ts)) timeMap.set(ts, { time: ts });
      const pt = timeMap.get(ts)!;
      pt[`In (Port ${q.port}) B/s`] = s.bytes_in_per_sec;
      pt[`Out (Port ${q.port}) B/s`] = s.bytes_out_per_sec;
    }
  }
  const chartData = Array.from(timeMap.values())
    .sort((a, b) => (a.time as number) - (b.time as number));

  if (chartData.length === 0) {
    return <p className="text-gray-500 dark:text-slate-400 text-center py-4 text-sm">No throughput data available yet</p>;
  }
  return (
    <ResponsiveContainer width="100%" height={200}>
      <LineChart data={chartData}>
        <CartesianGrid strokeDasharray="3 3" stroke={gridColor} />
        <XAxis dataKey="time" tick={{ fontSize: 10, fill: axisColor }}
          tickFormatter={(ts: number) => new Date(ts).toLocaleTimeString()} stroke={axisColor} />
        <YAxis tick={{ fontSize: 10, fill: axisColor }} tickFormatter={formatBps} stroke={axisColor} />
        <Tooltip formatter={(value: number) => formatBps(value)}
          labelFormatter={(ts: number) => new Date(ts).toLocaleString()}
          contentStyle={tooltipStyle}
          labelStyle={tooltipTextStyle}
          itemStyle={tooltipTextStyle} />
        {qualityList.map(q => (
          <React.Fragment key={q.port}>
            <Line type="monotone" dataKey={`In (Port ${q.port}) B/s`}
              stroke="#3b82f6" dot={false} strokeWidth={2} />
            <Line type="monotone" dataKey={`Out (Port ${q.port}) B/s`}
              stroke="#10b981" dot={false} strokeWidth={2} />
          </React.Fragment>
        ))}
      </LineChart>
    </ResponsiveContainer>
  );
};

export const TrojanPage = () => {
  const [timeRange, setTimeRange] = useState<ChartTimeRange>({
    preset: '1h',
    startMs: Date.now() - 3600000,
    endMs: Date.now(),
  });

  const handleTimeRangeChange = useCallback((range: ChartTimeRange) => {
    setTimeRange(range);
  }, []);

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
          <ChartContainer
            title="Throughput History"
            timeRange={timeRange}
            onTimeRangeChange={handleTimeRangeChange}
            isEmpty={qualityList.every(q => q.history.length === 0)}
          >
            <ThroughputHistory qualityList={qualityList} timeRange={timeRange} />
          </ChartContainer>
        </div>
      )}
    </div>
  );
};

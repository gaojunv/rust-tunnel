import React, { useMemo } from 'react';
import { LineChart, Line, XAxis, YAxis, CartesianGrid, Tooltip, Legend, ResponsiveContainer } from 'recharts';
import type { PortTraffic } from '../types';
import { ChartContainer } from './shared/ChartContainer';
import { TimeRangeSelector } from './shared/TimeRangeSelector';
import { useTimeRange } from '../hooks/useTimeRange';
import { formatBytes } from '../utils/format';
import { useMediaQuery } from '../hooks/useMediaQuery';
import { useTheme } from '../theme/ThemeProvider';

interface TrafficChartProps {
  traffic: PortTraffic[];
}

const colorPool = ['#3b82f6', '#10b981', '#8b5cf6', '#f59e0b', '#ef4444', '#06b6d4'];

export const TrafficChart = ({ traffic }: TrafficChartProps) => {
  const { resolvedTheme } = useTheme();
  const { range, preset, presets, setPreset, setCustomRange } = useTimeRange();
  const isSmallScreen = useMediaQuery('(max-width: 639px)');

  const isDark = resolvedTheme === 'dark';
  const axisColor = isDark ? '#94a3b8' : '#6b7280';
  const gridColor = isDark ? '#334155' : '#e5e7eb';
  const tooltipStyle = isDark
    ? { backgroundColor: '#1e293b', border: '1px solid #475569', color: '#f1f5f9' }
    : { backgroundColor: '#ffffff', border: '1px solid #e5e7eb', color: '#111827' };
  const tooltipTextStyle = { color: tooltipStyle.color };
  const legendStyle = isDark ? { color: '#e2e8f0' } : { color: '#111827' };

  const chartData = useMemo(() => {
    const timeMap = new Map<number, Record<string, number | string>>();

    for (const portTraffic of traffic) {
      for (const bucket of portTraffic.buckets) {
        const ts = new Date(bucket.timestamp).getTime();
        if (ts < range.startMs || ts > range.endMs) continue;
        if (!timeMap.has(ts)) {
          timeMap.set(ts, { time: ts });
        }
        const point = timeMap.get(ts)!;
        point[`in_${portTraffic.port}`] = bucket.bytes_in;
        point[`out_${portTraffic.port}`] = bucket.bytes_out;
      }
    }

    return Array.from(timeMap.values())
      .sort((a, b) => (a.time as number) - (b.time as number));
  }, [traffic, range]);

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-end">
        <TimeRangeSelector
          preset={preset}
          presets={presets}
          customStartMs={range.startMs}
          customEndMs={range.endMs}
          onPresetChange={setPreset}
          onCustomChange={setCustomRange}
        />
      </div>
      <ChartContainer title="Network Traffic">
        {chartData.length === 0 ? (
          <p className="text-gray-500 text-center py-8 dark:text-slate-400">No data available</p>
        ) : (
          <ResponsiveContainer width="100%" height={isSmallScreen ? 250 : 300}>
            <LineChart data={chartData}>
              <CartesianGrid strokeDasharray="3 3" stroke={gridColor} />
              <XAxis
                dataKey="time"
                tick={{ fontSize: isSmallScreen ? 9 : 12, fill: axisColor }}
                tickFormatter={(ts: number) => new Date(ts).toLocaleTimeString()}
                stroke={axisColor}
              />
              <YAxis
                tick={{ fontSize: isSmallScreen ? 9 : 12, fill: axisColor }}
                tickFormatter={formatBytes}
                width={70}
                stroke={axisColor}
              />
              <Tooltip
                formatter={(value: number) => formatBytes(value)}
                labelFormatter={(ts: number) => new Date(ts).toLocaleString()}
                contentStyle={tooltipStyle}
                labelStyle={tooltipTextStyle}
                itemStyle={tooltipTextStyle}
              />
              <Legend
                wrapperStyle={{ fontSize: isSmallScreen ? '10px' : '12px', ...legendStyle }}
              />
              {traffic.map((portTraffic, idx) => (
                <React.Fragment key={portTraffic.port}>
                  <Line
                    type="monotone"
                    dataKey={`in_${portTraffic.port}`}
                    name={`In (Port ${portTraffic.port})`}
                    stroke={colorPool[idx * 2 % colorPool.length]}
                    dot={false}
                    strokeWidth={2}
                  />
                  <Line
                    type="monotone"
                    dataKey={`out_${portTraffic.port}`}
                    name={`Out (Port ${portTraffic.port})`}
                    stroke={colorPool[(idx * 2 + 1) % colorPool.length]}
                    dot={false}
                    strokeWidth={2}
                  />
                </React.Fragment>
              ))}
            </LineChart>
          </ResponsiveContainer>
        )}
      </ChartContainer>
    </div>
  );
};

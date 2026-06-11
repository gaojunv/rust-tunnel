import React, { useState, useMemo, useCallback } from 'react';
import { LineChart, Line, XAxis, YAxis, CartesianGrid, Tooltip, Legend, ResponsiveContainer } from 'recharts';
import type { PortTraffic } from '../types';
import { ChartContainer } from './shared/ChartContainer';
import type { ChartTimeRange } from './shared/ChartContainer';
import { formatBytes } from '../utils/format';
import { useMediaQuery } from '../hooks/useMediaQuery';
import { useTheme } from '../theme/ThemeProvider';

interface TrafficChartProps {
  traffic: PortTraffic[];
}

const colorPool = ['#3b82f6', '#10b981', '#8b5cf6', '#f59e0b', '#ef4444', '#06b6d4'];

export const TrafficChart = ({ traffic }: TrafficChartProps) => {
  const { resolvedTheme } = useTheme();
  const [timeRange, setTimeRange] = useState<ChartTimeRange>({
    preset: '1h',
    startMs: Date.now() - 3600000,
    endMs: Date.now(),
  });
  const isSmallScreen = useMediaQuery('(max-width: 639px)');

  const isDark = resolvedTheme === 'dark';
  const axisColor = isDark ? '#94a3b8' : '#6b7280';
  const gridColor = isDark ? '#334155' : '#e5e7eb';
  const tooltipStyle = isDark
    ? { backgroundColor: '#1e293b', border: '1px solid #475569', color: '#f1f5f9' }
    : { backgroundColor: '#ffffff', border: '1px solid #e5e7eb', color: '#111827' };
  const legendStyle = isDark ? { color: '#e2e8f0' } : { color: '#111827' };

  const handleTimeRangeChange = useCallback((range: ChartTimeRange) => {
    setTimeRange(range);
  }, []);

  const chartData = useMemo(() => {
    const timeMap = new Map<number, Record<string, number | string>>();

    for (const portTraffic of traffic) {
      for (const bucket of portTraffic.buckets) {
        const ts = new Date(bucket.timestamp).getTime();
        if (ts < timeRange.startMs || ts > timeRange.endMs) continue;
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
  }, [traffic, timeRange]);

  return (
    <ChartContainer
      title="Network Traffic"
      timeRange={timeRange}
      onTimeRangeChange={handleTimeRangeChange}
      isEmpty={chartData.length === 0}
    >
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
            labelStyle={tooltipStyle}
            itemStyle={tooltipStyle}
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
    </ChartContainer>
  );
};

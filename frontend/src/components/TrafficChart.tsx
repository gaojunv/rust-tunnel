import React, { useState, useMemo, useCallback } from 'react';
import { LineChart, Line, XAxis, YAxis, CartesianGrid, Tooltip, Legend, ResponsiveContainer } from 'recharts';
import type { PortTraffic } from '../types';
import { ChartContainer } from './shared/ChartContainer';
import type { ChartTimeRange } from './shared/ChartContainer';
import { formatBytes } from '../utils/format';
import { useMediaQuery } from '../hooks/useMediaQuery';

interface TrafficChartProps {
  traffic: PortTraffic[];
}

const colorPool = ['#3b82f6', '#10b981', '#8b5cf6', '#f59e0b', '#ef4444', '#06b6d4'];

export const TrafficChart = ({ traffic }: TrafficChartProps) => {
  const [timeRange, setTimeRange] = useState<ChartTimeRange>({
    preset: '1h',
    startMs: Date.now() - 3600000,
    endMs: Date.now(),
  });
  const isSmallScreen = useMediaQuery('(max-width: 639px)');

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
          <CartesianGrid strokeDasharray="3 3" />
          <XAxis
            dataKey="time"
            tick={{ fontSize: isSmallScreen ? 9 : 12 }}
            tickFormatter={(ts: number) => new Date(ts).toLocaleTimeString()}
          />
          <YAxis
            tick={{ fontSize: isSmallScreen ? 9 : 12 }}
            tickFormatter={formatBytes}
            width={70}
          />
          <Tooltip
            formatter={(value: number) => formatBytes(value)}
            labelFormatter={(ts: number) => new Date(ts).toLocaleString()}
          />
          <Legend
            wrapperStyle={{ fontSize: isSmallScreen ? '10px' : '12px' }}
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

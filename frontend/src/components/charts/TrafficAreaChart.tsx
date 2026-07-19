import { useMemo } from 'react';
import { Area, AreaChart, CartesianGrid, XAxis, YAxis } from 'recharts';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import {
  ChartContainer,
  ChartLegend,
  ChartLegendContent,
  ChartTooltip,
  ChartTooltipContent,
  type ChartConfig,
} from '@/components/ui/chart';
import { TimeRangeSelector } from '@/components/shared/TimeRangeSelector';
import { useTimeRange } from '@/hooks/useTimeRange';
import { useAllTraffic } from '@/api/hooks';
import { formatBytes } from '@/utils/format';
import { ChartEmpty } from './ChartEmpty';

export const TrafficAreaChart = () => {
  const { data: traffic = [] } = useAllTraffic();
  const { range, preset, presets, setPreset, setCustomRange } = useTimeRange();

  const ports = useMemo(
    () => traffic.map((t) => t.port).sort((a, b) => a - b),
    [traffic],
  );

  const chartConfig = useMemo<ChartConfig>(() => {
    const config: ChartConfig = {};
    ports.forEach((port, idx) => {
      config[`port_${port}`] = {
        label: `Port ${port}`,
        color: `hsl(var(--chart-${(idx % 5) + 1}))`,
      };
    });
    return config;
  }, [ports]);

  const chartData = useMemo(() => {
    const timeMap = new Map<number, Record<string, number | string>>();
    for (const portTraffic of traffic) {
      for (const bucket of portTraffic.buckets) {
        const ts = new Date(bucket.timestamp).getTime();
        if (ts < range.startMs || ts > range.endMs) continue;
        if (!timeMap.has(ts)) {
          timeMap.set(ts, { time: ts });
        }
        timeMap.get(ts)![`port_${portTraffic.port}`] = bucket.bytes_in + bucket.bytes_out;
      }
    }
    return Array.from(timeMap.values()).sort(
      (a, b) => (a.time as number) - (b.time as number),
    );
  }, [traffic, range]);

  return (
    <Card>
      <CardHeader className="flex flex-col gap-3 space-y-0 sm:flex-row sm:items-center sm:justify-between">
        <CardTitle>Network Traffic</CardTitle>
        <TimeRangeSelector
          preset={preset}
          presets={presets}
          customStartMs={range.startMs}
          customEndMs={range.endMs}
          onPresetChange={setPreset}
          onCustomChange={setCustomRange}
        />
      </CardHeader>
      <CardContent>
        {chartData.length === 0 ? (
          <ChartEmpty message="No traffic data available" />
        ) : (
          <ChartContainer config={chartConfig} className="h-[250px] w-full sm:h-[300px]">
            <AreaChart data={chartData} margin={{ left: 12, right: 12 }}>
              <CartesianGrid strokeDasharray="3 3" vertical={false} />
              <XAxis
                dataKey="time"
                tickLine={false}
                axisLine={false}
                tickMargin={8}
                tickFormatter={(ts: number) =>
                  new Date(ts).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
                }
              />
              <YAxis
                tickLine={false}
                axisLine={false}
                tickMargin={8}
                width={70}
                tickFormatter={formatBytes}
              />
              <ChartTooltip
                content={
                  <ChartTooltipContent
                    labelFormatter={(ts) => new Date(Number(ts)).toLocaleString()}
                    formatter={(value, name) => {
                      const key = String(name);
                      return (
                        <div className="flex w-full items-center gap-2">
                          <span
                            className="h-2.5 w-2.5 shrink-0 rounded-[2px]"
                            style={{ backgroundColor: chartConfig[key]?.color }}
                          />
                          <span className="flex-1 text-muted-foreground">
                            {chartConfig[key]?.label ?? key}
                          </span>
                          <span className="font-mono font-medium tabular-nums text-foreground">
                            {formatBytes(Number(value))}
                          </span>
                        </div>
                      );
                    }}
                  />
                }
              />
              <ChartLegend content={<ChartLegendContent />} />
              {ports.map((port) => (
                <Area
                  key={port}
                  type="monotone"
                  dataKey={`port_${port}`}
                  stackId="total"
                  stroke={`var(--color-port_${port})`}
                  fill={`var(--color-port_${port})`}
                  fillOpacity={0.4}
                  strokeWidth={1.5}
                  dot={false}
                />
              ))}
            </AreaChart>
          </ChartContainer>
        )}
      </CardContent>
    </Card>
  );
};

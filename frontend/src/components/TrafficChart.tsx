import { Fragment, useMemo } from 'react';
import { LineChart, Line, XAxis, YAxis, CartesianGrid } from 'recharts';
import type { PortTraffic } from '../types';
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
import { useTimeRange } from '../hooks/useTimeRange';
import { formatBytes } from '../utils/format';

interface TrafficChartProps {
  traffic: PortTraffic[];
}

export const TrafficChart = ({ traffic }: TrafficChartProps) => {
  const { range, preset, presets, setPreset, setCustomRange } = useTimeRange();

  const chartConfig = useMemo<ChartConfig>(() => {
    const config: ChartConfig = {};
    traffic.forEach((portTraffic, idx) => {
      config[`in_${portTraffic.port}`] = {
        label: `In (Port ${portTraffic.port})`,
        color: `hsl(var(--chart-${((idx * 2) % 5) + 1}))`,
      };
      config[`out_${portTraffic.port}`] = {
        label: `Out (Port ${portTraffic.port})`,
        color: `hsl(var(--chart-${((idx * 2 + 1) % 5) + 1}))`,
      };
    });
    return config;
  }, [traffic]);

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
      <Card>
        <CardHeader>
          <CardTitle className="text-lg">Network Traffic</CardTitle>
        </CardHeader>
        <CardContent>
          {chartData.length === 0 ? (
            <p className="py-8 text-center text-muted-foreground">No data available</p>
          ) : (
            <ChartContainer config={chartConfig} className="h-[250px] w-full sm:h-[300px]">
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
                  tickFormatter={formatBytes}
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
                            {formatBytes(Number(value))}
                          </span>
                        </div>
                      )}
                    />
                  }
                />
                <ChartLegend content={<ChartLegendContent />} />
                {traffic.map((portTraffic) => (
                  <Fragment key={portTraffic.port}>
                    <Line
                      type="monotone"
                      dataKey={`in_${portTraffic.port}`}
                      stroke={`var(--color-in_${portTraffic.port})`}
                      dot={false}
                      strokeWidth={2}
                    />
                    <Line
                      type="monotone"
                      dataKey={`out_${portTraffic.port}`}
                      stroke={`var(--color-out_${portTraffic.port})`}
                      dot={false}
                      strokeWidth={2}
                    />
                  </Fragment>
                ))}
              </LineChart>
            </ChartContainer>
          )}
        </CardContent>
      </Card>
    </div>
  );
};

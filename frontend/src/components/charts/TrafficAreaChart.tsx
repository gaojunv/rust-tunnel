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
import { useStatsQuery } from '@/api/hooks';
import { formatBps } from '@/utils/format';
import { ChartEmpty } from './ChartEmpty';

const ENTITY_TYPES = ['client', 'proxy', 'shadowsocks', 'trojan'];

export const TrafficAreaChart = () => {
  const { range, preset, presets, setPreset, setCustomRange } = useTimeRange();

  const startIso = useMemo(() => new Date(range.startMs).toISOString(), [range.startMs]);
  const endIso = useMemo(() => new Date(range.endMs).toISOString(), [range.endMs]);
  const { data: snapshots = [] } = useStatsQuery(ENTITY_TYPES, undefined, startIso, endIso);

  const entities = useMemo(
    () => Array.from(new Set(snapshots.map((s) => s.entity_id))).sort(),
    [snapshots],
  );

  const seriesKeys = useMemo(
    () => entities.map((_, idx) => `entity_${idx}`),
    [entities],
  );

  const chartConfig = useMemo<ChartConfig>(() => {
    const config: ChartConfig = {};
    entities.forEach((entityId, idx) => {
      config[seriesKeys[idx]] = {
        label: entityId,
        color: `hsl(var(--chart-${(idx % 5) + 1}))`,
      };
    });
    return config;
  }, [entities, seriesKeys]);

  const chartData = useMemo(() => {
    const keyOf = new Map(entities.map((id, idx) => [id, seriesKeys[idx]]));
    const timeMap = new Map<number, Record<string, number | string>>();
    for (const snap of snapshots) {
      const ts = new Date(snap.timestamp).getTime();
      if (!timeMap.has(ts)) {
        timeMap.set(ts, { time: ts });
      }
      timeMap.get(ts)![keyOf.get(snap.entity_id)!] =
        snap.bytes_in_rate + snap.bytes_out_rate;
    }
    return Array.from(timeMap.values()).sort(
      (a, b) => (a.time as number) - (b.time as number),
    );
  }, [snapshots, entities, seriesKeys]);

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
                tickFormatter={formatBps}
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
                            {formatBps(Number(value))}
                          </span>
                        </div>
                      );
                    }}
                  />
                }
              />
              <ChartLegend content={<ChartLegendContent />} />
              {seriesKeys.map((key) => (
                <Area
                  key={key}
                  type="monotone"
                  dataKey={key}
                  stackId="total"
                  stroke={`var(--color-${key})`}
                  fill={`var(--color-${key})`}
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

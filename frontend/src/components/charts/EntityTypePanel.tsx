import { useMemo } from 'react';
import { useTranslation } from 'react-i18next';
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
import { useEntityLabel, type EntityType } from '@/hooks/useEntityLabel';
import { formatBps } from '@/utils/format';
import type { StatsSnapshot } from '@/types';
import { ChartEmpty } from './ChartEmpty';
import { createYAxisTick } from './yAxisTick';

interface EntityTypePanelProps {
  type: EntityType;
  titleLabel: string;
  snapshots: StatsSnapshot[];
}

/**
 * 单个 entity_type 的堆叠 AreaChart 子图。
 * 内部按 entity_id 拆 series，label 通过 useEntityLabel 映射为人类可读名。
 */
export const EntityTypePanel = ({ type, titleLabel, snapshots }: EntityTypePanelProps) => {
  const { t } = useTranslation();
  const entityLabel = useEntityLabel();

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
        label: entityLabel(type, entityId),
        color: `hsl(var(--chart-${(idx % 5) + 1}))`,
      };
    });
    return config;
  }, [entities, seriesKeys, entityLabel, type]);

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
      <CardHeader className="pb-2">
        <CardTitle className="text-sm">{titleLabel}</CardTitle>
      </CardHeader>
      <CardContent>
        {chartData.length === 0 ? (
          <ChartEmpty message={t('dashboard.noTraffic', { type: titleLabel })} />
        ) : (
          <ChartContainer config={chartConfig} className="h-[220px] w-full">
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
                tick={createYAxisTick(formatBps)}
              />
              <ChartTooltip
                content={
                  <ChartTooltipContent
                    labelFormatter={(_value, payload) => {
                      // 修复：shadcn wrapper 在 label 为 number（时间戳）时会误取
                      // 首个 series 的 label；这里直接从 payload 取时间戳。
                      const ts = payload?.[0]?.payload?.time as number | undefined;
                      return ts ? new Date(ts).toLocaleString() : '';
                    }}
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

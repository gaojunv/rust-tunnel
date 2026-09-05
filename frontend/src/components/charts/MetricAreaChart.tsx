import { useId, useMemo } from 'react';
import { Area, AreaChart, CartesianGrid, ReferenceLine, XAxis, YAxis } from 'recharts';
import {
  ChartContainer,
  ChartLegend,
  ChartLegendContent,
  ChartTooltip,
  ChartTooltipContent,
  type ChartConfig,
} from '@/components/ui/chart';
import { ChartEmpty } from './ChartEmpty';
import { createYAxisTick } from './yAxisTick';

export interface MetricSeries {
  dataKey: string;
  label: string;
  colorVar: string; // e.g. 'hsl(var(--chart-1))'
}

interface MetricAreaChartProps {
  data: Array<Record<string, number | string>>;
  xKey?: string;
  series: MetricSeries[];
  yFormatter: (value: number) => string;
  threshold?: number;
  thresholdLabel?: string;
  className?: string;
  emptyText?: string;
}

export const MetricAreaChart = ({
  data,
  xKey = 'timestamp',
  series,
  yFormatter,
  threshold,
  thresholdLabel,
  className = 'h-[200px] w-full',
  emptyText,
}: MetricAreaChartProps) => {
  const gradientId = useId().replace(/:/g, '');

  const chartConfig = useMemo<ChartConfig>(() => {
    const config: ChartConfig = {};
    for (const s of series) {
      config[s.dataKey] = { label: s.label, color: s.colorVar };
    }
    return config;
  }, [series]);

  if (data.length === 0) {
    return <ChartEmpty message={emptyText} />;
  }

  return (
    <ChartContainer config={chartConfig} className={className}>
      <AreaChart data={data} margin={{ left: 12, right: 12 }}>
        <defs>
          {series.map((s) => (
            <linearGradient
              key={s.dataKey}
              id={`${gradientId}-${s.dataKey}`}
              x1="0"
              y1="0"
              x2="0"
              y2="1"
            >
              <stop offset="0%" stopColor={s.colorVar} stopOpacity={0.3} />
              <stop offset="100%" stopColor={s.colorVar} stopOpacity={0} />
            </linearGradient>
          ))}
        </defs>
        <CartesianGrid strokeDasharray="3 3" vertical={false} />
        <XAxis
          dataKey={xKey}
          tickLine={false}
          axisLine={false}
          tickMargin={8}
          tickFormatter={(ts: string) =>
            new Date(ts).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
          }
        />
        <YAxis
          tickLine={false}
          axisLine={false}
          tickMargin={8}
          width={70}
          tick={createYAxisTick(yFormatter)}
        />
        <ChartTooltip
          content={
            <ChartTooltipContent
              labelFormatter={(ts) => new Date(String(ts)).toLocaleString()}
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
                      {yFormatter(Number(value))}
                    </span>
                  </div>
                );
              }}
            />
          }
        />
        {series.length > 1 && <ChartLegend content={<ChartLegendContent />} />}
        {threshold !== undefined && (
          <ReferenceLine
            y={threshold}
            stroke="hsl(var(--destructive))"
            strokeDasharray="4 4"
            label={
              thresholdLabel
                ? { value: thresholdLabel, position: 'insideTopRight', fontSize: 10, fill: 'hsl(var(--muted-foreground))' }
                : undefined
            }
          />
        )}
        {series.map((s) => (
          <Area
            key={s.dataKey}
            type="monotone"
            dataKey={s.dataKey}
            stroke={`var(--color-${s.dataKey})`}
            fill={`url(#${gradientId}-${s.dataKey})`}
            strokeWidth={2}
            dot={false}
          />
        ))}
      </AreaChart>
    </ChartContainer>
  );
};

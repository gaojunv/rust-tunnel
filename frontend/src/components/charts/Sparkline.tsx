import { useId } from 'react';
import { Area, AreaChart } from 'recharts';
import { ChartContainer, type ChartConfig } from '@/components/ui/chart';

interface SparklineProps {
  values: number[];
  colorVar?: string;
  className?: string;
}

export const Sparkline = ({
  values,
  colorVar = 'hsl(var(--chart-1))',
  className = 'h-8 w-full',
}: SparklineProps) => {
  const gradientId = useId().replace(/:/g, '');

  if (values.length === 0) {
    return null;
  }

  const data = values.map((v, i) => ({ i, v }));
  const config: ChartConfig = { v: { label: 'value', color: colorVar } };

  return (
    <ChartContainer config={config} className={className}>
      <AreaChart data={data} margin={{ top: 2, bottom: 2, left: 0, right: 0 }}>
        <defs>
          <linearGradient id={`${gradientId}-v`} x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor={colorVar} stopOpacity={0.3} />
            <stop offset="100%" stopColor={colorVar} stopOpacity={0} />
          </linearGradient>
        </defs>
        <Area
          type="monotone"
          dataKey="v"
          stroke="var(--color-v)"
          fill={`url(#${gradientId}-v)`}
          strokeWidth={1.5}
          dot={false}
          isAnimationActive={false}
        />
      </AreaChart>
    </ChartContainer>
  );
};

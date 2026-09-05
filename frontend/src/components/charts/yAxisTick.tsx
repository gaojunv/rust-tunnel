import type { ReactElement } from 'react';
import type { YAxisTickContentProps } from 'recharts';

/**
 * 自定义 Y 轴刻度渲染：输出普通 <text>（不传 width）。
 * recharts 默认 Text 组件会按空格分词换行，"1.34 MB/s" 这类标签
 * 在固定轴宽内会被挤压成两行；这里禁用换行，标签始终单行显示。
 * recharts 3 中 YAxis 的函数型 tick 经 TickItem 以 className: combinedClassName
 * 注入（值为 recharts-cartesian-axis-tick-value），需透传该 className
 * 以命中 ChartContainer 中的主题选择器；同时用 fill 属性兜底保证暗黑可读。
 * 在 recharts 3 中 YAxis tick 接收 YAxisTickContentProps（x/y 为 number|string），
 * 因此直接适配该类型以避免 TickProp 兼容报错。
 */
export const createYAxisTick =
  (formatter: (value: number) => string) =>
  ({
    x = 0,
    y = 0,
    payload,
    className,
  }: YAxisTickContentProps & { className?: string }): ReactElement => (
    <text
      x={x as number}
      y={y as number}
      dy={4}
      textAnchor="end"
      fontSize={12}
      fill="hsl(var(--muted-foreground))"
      className={className ?? 'recharts-cartesian-axis-tick-value'}
    >
      {formatter(Number(payload?.value ?? 0))}
    </text>
  );

import type { ReactElement } from 'react';

interface YAxisTickProps {
  x?: number;
  y?: number;
  payload?: { value: number | string };
}

/**
 * 自定义 Y 轴刻度渲染：输出普通 <text>（不传 width）。
 * recharts 默认 Text 组件会按空格分词换行，"1.34 MB/s" 这类标签
 * 在固定轴宽内会被挤压成两行；这里禁用换行，标签始终单行显示。
 * className 复用 ChartContainer 中既有的刻度颜色样式。
 */
export const createYAxisTick =
  (formatter: (value: number) => string) =>
  ({ x = 0, y = 0, payload }: YAxisTickProps): ReactElement => (
    <text
      x={x}
      y={y}
      dy={4}
      textAnchor="end"
      fontSize={12}
      className="recharts-cartesian-axis-tick_text"
    >
      {formatter(Number(payload?.value ?? 0))}
    </text>
  );

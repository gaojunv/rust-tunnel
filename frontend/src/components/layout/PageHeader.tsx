import { useRef } from 'react';
import { cn } from '@/lib/utils';
import { TitleEffectSwitch } from '@/components/shared/TitleEffectSwitch';
import { usePreferences } from '@/preferences/PreferencesProvider';
import { useGridWaveCanvas } from '@/components/shared/useGridWaveCanvas';

interface PageHeaderProps {
  title: string;
  description?: string;
  children?: React.ReactNode;
  className?: string;
}

// 极光悬浮卡片式页头：玻璃拟态卡片 + 品牌色极光光斑（缓慢漂移）
// + 细网格纹理 + 柔和投影/顶部高光，营造立体感与背景氛围。
// 光斑透明度在暗色主题下更高（暗色下辉光更明显），动画遵循
// prefers-reduced-motion（见 index.css 中的 .animate-aurora*）。
// grid-wave 模式下，静态 grid-texture 被替换为覆盖整个卡片的动态网格画布
// （由 useGridWaveCanvas 驱动），鼠标在卡片任意位置都能激起涟漪。
export function PageHeader({ title, description, children, className }: PageHeaderProps) {
  // 整个页头卡片作为标题动画的指针事件宿主：鼠标在卡片任意位置（含描述、
  // 右侧按钮区）移动都能驱动标题的光波与扰动，避免移出文字边缘效果就中断。
  const cardRef = useRef<HTMLDivElement>(null);
  const gridCanvasRef = useRef<HTMLCanvasElement>(null);
  const { prefs } = usePreferences();
  const isGridWave = prefs.titleEffect === 'grid-wave';

  // grid-wave 模式：canvas 覆盖整个卡片，hook 负责网格构建 + 涟漪渲染。
  // 非 grid-wave 模式下 active=false 早退；切换 titleEffect 时 active 变化
  // 触发 effect 清理并重建。
  useGridWaveCanvas(gridCanvasRef, cardRef, isGridWave);

  return (
    <div
      ref={cardRef}
      className={cn(
        'glass-card relative flex flex-col gap-4 overflow-hidden rounded-2xl border border-border/60 px-6 py-5 sm:flex-row sm:items-center sm:justify-between sm:px-8 sm:py-6',
        // 立体感：顶部内侧高光 + 柔和投影（移动端去掉品牌色辉光外投影，大屏恢复）
        'shadow-[inset_0_1px_0_0_hsl(var(--foreground)/0.05),0_2px_8px_-4px_hsl(var(--foreground)/0.08)] md:shadow-[inset_0_1px_0_0_hsl(var(--foreground)/0.05),0_12px_32px_-16px_hsl(var(--primary)/0.28)]',
        className
      )}
    >
      {/* 氛围背景：极光光斑 + （网格纹理 | 动态网格画布）—— 纯装饰，不响应交互 */}
      <div aria-hidden className="pointer-events-none absolute inset-0">
        <div className="aurora-blob animate-aurora -left-12 -top-24 h-60 w-96 bg-[radial-gradient(circle,hsl(var(--primary)/0.28),transparent_70%)] dark:bg-[radial-gradient(circle,hsl(var(--primary)/0.42),transparent_70%)]" />
        <div className="aurora-blob animate-aurora-alt -bottom-28 right-0 h-56 w-80 bg-[radial-gradient(circle,hsl(var(--chart-2)/0.22),transparent_70%)] dark:bg-[radial-gradient(circle,hsl(var(--chart-2)/0.34),transparent_70%)]" />
        {isGridWave ? (
          <canvas ref={gridCanvasRef} className="absolute inset-0" />
        ) : (
          <div className="grid-texture absolute inset-0" />
        )}
      </div>

      <div className="relative">
        {/* flex + items-center：让 TitleEffectSwitch 的三种模式（纯 inline span /
            inline-block 包 canvas）都以 flex item 形式布局，高度计算一致，
            避免切换 titleEffect 时 h1 行盒受 baseline/strut 影响而轻微变化。 */}
        <h1 className="flex items-center text-[32px] font-bold leading-tight tracking-tight">
          <TitleEffectSwitch text={title} eventTargetRef={cardRef} />
        </h1>
        {description && <p className="mt-1.5 text-muted-foreground">{description}</p>}
      </div>
      {children && <div className="relative flex items-center gap-2">{children}</div>}
    </div>
  );
}

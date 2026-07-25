import { cn } from '@/lib/utils';

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
export function PageHeader({ title, description, children, className }: PageHeaderProps) {
  return (
    <div
      className={cn(
        'glass-card relative flex flex-col gap-4 overflow-hidden rounded-2xl border border-border/60 px-6 py-5 sm:flex-row sm:items-center sm:justify-between sm:px-8 sm:py-6',
        // 立体感：顶部内侧高光 + 品牌色柔和投影
        'shadow-[inset_0_1px_0_0_hsl(var(--foreground)/0.05),0_12px_32px_-16px_hsl(var(--primary)/0.28)]',
        className
      )}
    >
      {/* 氛围背景：极光光斑 + 网格纹理（纯装饰，不响应交互） */}
      <div aria-hidden className="pointer-events-none absolute inset-0">
        <div className="aurora-blob animate-aurora -left-12 -top-20 h-52 w-80 bg-[radial-gradient(circle,hsl(var(--primary)/0.22),transparent_70%)] dark:bg-[radial-gradient(circle,hsl(var(--primary)/0.35),transparent_70%)]" />
        <div className="aurora-blob animate-aurora-alt -bottom-24 right-0 h-48 w-72 bg-[radial-gradient(circle,hsl(var(--chart-2)/0.16),transparent_70%)] dark:bg-[radial-gradient(circle,hsl(var(--chart-2)/0.28),transparent_70%)]" />
        <div className="grid-texture absolute inset-0" />
      </div>

      <div className="relative">
        <h1 className="text-2xl font-bold tracking-tight text-gradient">{title}</h1>
        {description && <p className="text-muted-foreground">{description}</p>}
      </div>
      {children && <div className="relative flex items-center gap-2">{children}</div>}
    </div>
  );
}

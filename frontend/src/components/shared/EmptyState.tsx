import type { ReactNode } from 'react';
import { Inbox, type LucideIcon } from 'lucide-react';
import { cn } from '@/lib/utils';

export interface EmptyStateProps {
  /** 展示图标，默认为 Inbox。 */
  icon?: LucideIcon;
  title: string;
  description?: string;
  /** 可选的操作按钮（如新建/前往），渲染在描述下方。 */
  action?: ReactNode;
  className?: string;
}

/**
 * 应用标准空态（EmptyState）——列表/表格/面板无数据时的统一展示。
 * 居中图标 + 标题 + 可选描述 + 可选操作，样式对齐 Dashboard / ProxyRuleTable 既有惯例。
 * Phase 5 会将各处手写的空态逐步迁移到本组件，新增页面请直接使用本组件，不要再手写。
 */
export function EmptyState({ icon: Icon = Inbox, title, description, action, className }: EmptyStateProps) {
  return (
    <div className={cn('flex flex-col items-center justify-center gap-2 py-10 text-center', className)}>
      <Icon className="h-8 w-8 text-muted-foreground opacity-40" aria-hidden="true" />
      <p className="text-sm font-medium">{title}</p>
      {description && <p className="text-sm text-muted-foreground">{description}</p>}
      {action && <div className="mt-2">{action}</div>}
    </div>
  );
}

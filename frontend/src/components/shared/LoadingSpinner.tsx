import { Loader2 } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';

export interface LoadingSpinnerProps {
  /** 提示文案，默认为 t('common.loading')。 */
  text?: string;
  className?: string;
}

/**
 * 不确定时长 / 流式等待的加载指示器（Loader2 旋转图标 + 可选文案）。
 *
 * 应用加载约定：已知形状的初始加载（表格/卡片/表单）用 ui/Skeleton 匹配最终布局，
 * 避免布局跳动；不确定时长/流式等待用 LoadingSpinner。
 */
export function LoadingSpinner({ text, className }: LoadingSpinnerProps) {
  const { t } = useTranslation();
  return (
    <div className={cn('flex items-center justify-center gap-2 py-8 text-muted-foreground', className)}>
      <Loader2 className="h-4 w-4 animate-spin" aria-hidden="true" />
      <span className="text-sm">{text ?? t('common.loading')}</span>
    </div>
  );
}

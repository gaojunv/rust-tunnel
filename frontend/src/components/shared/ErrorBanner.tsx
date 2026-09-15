import { RotateCcw } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';
import { resolveErrorMessage } from './ErrorState';

export interface ErrorBannerProps {
  /** 原始错误，经 resolveErrorMessage 解析为展示文案。 */
  error?: unknown;
  /** 直接指定展示文案，覆盖 error 解析结果。 */
  message?: string;
  /** 提供时渲染右侧小型重试图标按钮。 */
  onRetry?: () => void;
  className?: string;
}

/**
 * 行内错误横幅（ErrorBanner）——区块级错误提示，保持周围内容可见。
 * 适用于“部分失败但不必整面板替换”的场景（如 UsageTab 统计卡/表格/日志三处横幅）。
 * 样式对齐 UsageTab 既有惯例：rounded-lg + destructive 边框/底色/文字。
 *
 * i18n 说明：与 ErrorState 相同，本组件是 components/shared 层的应用复合组件，
 * 允许内部调用 useTranslation() 提供默认文案（common.loadFailed / common.retry），
 * 调用方可经 message prop 覆盖（对比 ui/confirm-dialog.tsx 的纯度规则）。
 */
export function ErrorBanner({ error, message, onRetry, className }: ErrorBannerProps) {
  const { t } = useTranslation();
  const resolved = message ?? resolveErrorMessage(error, t('common.loadFailed'));
  return (
    <div
      role="alert"
      className={cn(
        'flex items-center gap-2 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive',
        className,
      )}
    >
      <span className="min-w-0 flex-1">{resolved}</span>
      {onRetry && (
        <button
          type="button"
          onClick={onRetry}
          aria-label={t('common.retry')}
          className="shrink-0 rounded p-1 hover:bg-destructive/20"
        >
          <RotateCcw className="h-3.5 w-3.5" aria-hidden="true" />
        </button>
      )}
    </div>
  );
}

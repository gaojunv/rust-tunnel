import { AlertTriangle } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { getApiErrorMessage } from '@/api/client';
import { cn } from '@/lib/utils';
import { Button } from '@/components/ui/button';

export interface ErrorStateProps {
  /** 原始错误，经 resolveErrorMessage 解析为展示文案。 */
  error?: unknown;
  /** 直接指定展示文案，覆盖 error 解析结果。 */
  message?: string;
  /** 提供时渲染“重试”按钮。 */
  onRetry?: () => void;
  className?: string;
}

/**
 * 从原始错误中解析出可展示的文案；无法识别时回退到 fallback。
 * getApiErrorMessage 对无法识别的对象会回退为 String(err)（如 "[object Object]"），
 * 这种结果无展示意义，因此同样回退到 fallback（通常是 t('common.loadFailed')）。
 */
export function resolveErrorMessage(error: unknown, fallback: string): string {
  if (error === undefined || error === null) return fallback;
  const resolved = getApiErrorMessage(error);
  if (resolved === '' || resolved === '[object Object]') return fallback;
  return resolved;
}

/**
 * 整面板错误态（ErrorState）——替代各处复制粘贴的“错误文案 + 重试按钮”代码。
 * 布局与 EmptyState 对齐：居中图标 + 文案 + 可选重试。
 *
 * i18n 说明：本组件是 components/shared 层的应用复合组件（非纯 ui/ 原子组件），
 * 因此允许内部调用 useTranslation() 提供默认文案（common.loadFailed / common.retry），
 * 调用方可经 message prop 覆盖。这与 ui/confirm-dialog.tsx 的纯度规则相反——
 * 后者是纯 ui/ 层组件，不调用 useTranslation，文案由调用方经 t() 传入。
 */
export function ErrorState({ error, message, onRetry, className }: ErrorStateProps) {
  const { t } = useTranslation();
  const resolved = message ?? resolveErrorMessage(error, t('common.loadFailed'));
  return (
    <div className={cn('flex flex-col items-center justify-center gap-2 py-10 text-center', className)}>
      <AlertTriangle className="h-8 w-8 text-destructive opacity-60" aria-hidden="true" />
      <p className="text-sm text-destructive">{resolved}</p>
      {onRetry && (
        <Button variant="outline" size="sm" onClick={onRetry}>
          {t('common.retry')}
        </Button>
      )}
    </div>
  );
}

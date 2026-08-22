import { useTranslation } from 'react-i18next';
import {
  ConfirmDialog as BaseConfirmDialog,
  useConfirm as useBaseConfirm,
  type ConfirmPayload,
} from '@/components/ui/confirm-dialog';

/** LLM 侧专用薄封装：把 t('common.cancel/confirm') 透传到通用 ConfirmDialog 的按钮上。 */
export function ConfirmDialog({
  open,
  payload,
  onConfirm,
  onCancel,
  variant = 'destructive',
}: {
  open: boolean;
  payload: ConfirmPayload | null;
  onConfirm: () => void;
  onCancel: () => void;
  variant?: 'default' | 'destructive';
}) {
  const { t } = useTranslation();
  return (
    <BaseConfirmDialog
      open={open}
      payload={payload}
      onConfirm={onConfirm}
      onCancel={onCancel}
      variant={variant}
      confirmLabel={t('common.confirm')}
      cancelLabel={t('common.cancel')}
    />
  );
}

export function useConfirm() {
  return useBaseConfirm();
}

export type { ConfirmPayload };

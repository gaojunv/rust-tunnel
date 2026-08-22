import { useCallback, useState } from 'react';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';

export interface ConfirmPayload {
  title: string;
  description: string;
  detail?: string;
  confirmLabel?: string;
}

/**
 * 通用确认对话框（复用项目既有 Dialog，与 git/shared 的 ApprovalDialog 同范式）。
 * 用于替换 LLM 侧的 window.confirm 调用，提供 i18n 与无障碍语义。
 */
export function ConfirmDialog({
  open,
  payload,
  onConfirm,
  onCancel,
  variant = 'default',
  confirmLabel,
  cancelLabel,
}: {
  open: boolean;
  payload: ConfirmPayload | null;
  onConfirm: () => void;
  onCancel: () => void;
  variant?: 'default' | 'destructive';
  confirmLabel?: string;
  cancelLabel?: string;
}) {
  if (!payload) return null;
  return (
    <Dialog open={open} onOpenChange={(isOpen) => !isOpen && onCancel()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{payload.title}</DialogTitle>
          {payload.description && <DialogDescription>{payload.description}</DialogDescription>}
        </DialogHeader>
        {payload.detail && (
          <pre className="max-h-40 overflow-auto rounded-md bg-muted p-2 font-mono text-xs whitespace-pre-wrap break-all">
            {payload.detail}
          </pre>
        )}
        <DialogFooter>
          <Button variant="ghost" size="sm" onClick={onCancel}>
            {cancelLabel ?? 'Cancel'}
          </Button>
          <Button
            variant={variant === 'destructive' ? 'destructive' : 'default'}
            size="sm"
            onClick={onConfirm}
          >
            {confirmLabel ?? payload.confirmLabel ?? 'Confirm'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/**
 * ConfirmDialog 的按钮文案通过 i18n 传入（调用方用 t('common.cancel/t('common.confirm') 提供）。
 * 组件自身不调用 useTranslation，避免把 locale 依赖固化在通用组件内。
 */
/** 轻量 hook：管理 ConfirmDialog 的 {open, payload, confirm} 三元组。 */
export function useConfirm() {
  const [payload, setPayload] = useState<ConfirmPayload | null>(null);
  const [onOk, setOnOk] = useState<(() => void) | null>(null);
  const open = payload !== null;

  const confirm = useCallback((next: ConfirmPayload, fn: () => void) => {
    setPayload(next);
    setOnOk(() => fn);
  }, []);

  const cancel = useCallback(() => {
    setPayload(null);
    setOnOk(null);
  }, []);

  const confirmAndClose = useCallback(() => {
    const fn = onOk;
    setPayload(null);
    setOnOk(null);
    fn?.();
  }, [onOk]);

  return { open, payload, confirm, cancel, confirmAndClose };
}

import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";

type Props = {
  title: string;
  label: string;
  initial?: string;
  placeholder?: string;
  hint?: string;
  submitText?: string;
  onSubmit: (value: string) => void | Promise<void>;
  validate?: (value: string) => string | null;
  onClose: () => void;
};

export function NoteFormDialog({
  title,
  label,
  initial = "",
  placeholder,
  hint,
  submitText = "确定",
  onSubmit,
  validate,
  onClose,
}: Props) {
  const [value, setValue] = useState(initial);
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
    // 选中已有文本，方便重命名时直接改
    inputRef.current?.select();
  }, []);

  // Esc 关闭
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  const handleSubmit = useCallback(async () => {
    if (submitting) return;
    const syncErr = validate?.(value) ?? null;
    if (syncErr) {
      setError(syncErr);
      return;
    }
    setError(null);
    setSubmitting(true);
    try {
      await onSubmit(value);
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      setError(msg);
    } finally {
      setSubmitting(false);
    }
  }, [value, validate, onSubmit, submitting]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLInputElement>) => {
      if (e.key === "Enter") {
        e.preventDefault();
        void handleSubmit();
      }
    },
    [handleSubmit],
  );

  const overlay = (
    <div
      data-modal-open=""
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        className="w-[min(92vw,420px)] rounded-lg border border-border bg-popover p-4 shadow-xl"
        onMouseDown={(e) => e.stopPropagation()}
        onClick={(e) => e.stopPropagation()}
      >
        <h2 className="text-sm font-semibold">{title}</h2>
        <label className="mt-3 block text-xs font-medium text-muted-foreground">
          {label}
        </label>
        <Input
          ref={inputRef}
          value={value}
          onChange={(e) => {
            setValue(e.target.value);
            if (error) setError(null);
          }}
          onKeyDown={handleKeyDown}
          placeholder={placeholder}
          disabled={submitting}
          className="mt-1.5"
        />
        {hint && !error && <p className="mt-1.5 text-xs text-muted-foreground">{hint}</p>}
        {error && <p className="mt-1.5 text-xs text-destructive">{error}</p>}
        <div className="mt-4 flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={onClose} disabled={submitting}>
            取消
          </Button>
          <Button type="button" onClick={() => void handleSubmit()} disabled={submitting}>
            {submitting ? "处理中…" : submitText}
          </Button>
        </div>
      </div>
    </div>
  );

  return createPortal(overlay, document.body);
}

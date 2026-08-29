import { useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card, CardContent } from '@/components/ui/card';
import { FileUp, Loader2 } from 'lucide-react';

export const TEXT_MAX_BYTES = 2 * 1024 * 1024;
export const BINARY_MAX_BYTES = 20 * 1024 * 1024;
export const ACCEPTED_EXTENSIONS = ['md', 'txt', 'pdf', 'docx', 'xlsx', 'pptx'];
export const TEXT_EXTENSIONS = ['md', 'txt'];
/** 「正在处理」覆盖状态的过期 TTL：SSE 终态事件丢失（断线/丢帧）时，processing
 *  override 会永久假卡。30s 后移除 override 并失效文档查询，让 UI 回退到服务端
 *  DB 状态（真实 status），用户也可手动重试/刷新。 */
export const PROCESSING_TTL_MS = 30_000;

export function maxBytesFor(ext: string): number {
  return TEXT_EXTENSIONS.includes(ext) ? TEXT_MAX_BYTES : BINARY_MAX_BYTES;
}

function formatMax(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${Math.round(bytes / (1024 * 1024))}MB`;
  if (bytes >= 1024) return `${Math.round(bytes / 1024)}KB`;
  return `${bytes}B`;
}

export const ACCEPT_STRING = ACCEPTED_EXTENSIONS.map((e) => `.${e}`).join(',');

// 上传中条目（逐文件反馈）
export interface PendingUpload {
  id: string;
  name: string;
  status: 'uploading' | 'failed';
  reason?: string;
}

export interface DocUploadZoneLabels {
  uploadHint: string;
  browse: string;
  fileInvalid: string;
}

interface Props {
  onUpload: (file: File) => unknown;
  labels: DocUploadZoneLabels;
  isUploading?: boolean;
  disabled?: boolean;
  /** 上传失败文案格式化。**调用方应始终提供**：缺省时 onUpload 的 rejection 仍会被
   *  catch（避免 unhandled rejection），但错误不会呈现给用户，等于静默失败。 */
  formatUploadError?: (err: unknown) => string;
}

/** 共享文档上传区：文件选择 + 拖拽 + 大小/扩展名校验 + 调用上传。文案通过 labels 注入，避免硬编码 kb./wiki. 前缀。 */
export default function DocUploadZone({
  onUpload,
  labels,
  isUploading,
  disabled,
  formatUploadError,
}: Props) {
  const { t } = useTranslation();
  const [dragging, setDragging] = useState(false);
  const [pending, setPending] = useState<PendingUpload[]>([]);
  const fileInputRef = useRef<HTMLInputElement>(null);

  const handleFiles = (list: FileList | null) => {
    if (!list || list.length === 0) return;
    const invalidEntries: PendingUpload[] = [];
    const accepted: File[] = [];
    Array.from(list).forEach((f, idx) => {
      const ext = f.name.toLowerCase().split('.').pop() ?? '';
      if (!ACCEPTED_EXTENSIONS.includes(ext)) {
        const reason = t('ks.uploadReasonExt');
        invalidEntries.push({ id: `invalid-${Date.now()}-${idx}`, name: f.name, status: 'failed', reason: t('ks.uploadInvalidFile', { name: f.name, reason }) });
      } else if (f.size > maxBytesFor(ext)) {
        const reason = t('ks.uploadReasonSize', { max: formatMax(maxBytesFor(ext)) });
        invalidEntries.push({ id: `invalid-${Date.now()}-${idx}`, name: f.name, status: 'failed', reason: t('ks.uploadInvalidFile', { name: f.name, reason }) });
      } else {
        accepted.push(f);
      }
    });
    if (invalidEntries.length > 0) {
      setPending((prev) => [...prev, ...invalidEntries]);
    }
    accepted.forEach((f) => {
      const id = `${Date.now()}-${f.name}-${Math.random().toString(36).slice(2, 6)}`;
      setPending((prev) => [...prev, { id, name: f.name, status: 'uploading' }]);
      try {
        const result = onUpload(f);
        if (result instanceof Promise) {
          result.then(() => {
            setPending((prev) => prev.filter((p) => p.id !== id));
          }).catch((err: unknown) => {
            const reason = formatUploadError ? formatUploadError(err) : String(err);
            setPending((prev) => prev.map((p) => (p.id === id ? { ...p, status: 'failed', reason } : p)));
          });
        } else {
          setPending((prev) => prev.filter((p) => p.id !== id));
        }
      } catch (err: unknown) {
        const reason = formatUploadError ? formatUploadError(err) : String(err);
        setPending((prev) => prev.map((p) => (p.id === id ? { ...p, status: 'failed', reason } : p)));
      }
    });
  };

  const onKeyDown: React.KeyboardEventHandler<HTMLDivElement> = (e) => {
    if (disabled) return;
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      fileInputRef.current?.click();
    }
  };

  return (
    <Card>
      <CardContent className="p-4">
        <input
          ref={fileInputRef}
          type="file"
          accept={ACCEPT_STRING}
          multiple
          className="hidden"
          disabled={disabled}
          onChange={(e) => {
            handleFiles(e.target.files);
            e.target.value = '';
          }}
        />
        <div
          tabIndex={disabled ? -1 : 0}
          aria-disabled={disabled}
          onKeyDown={onKeyDown}
          className={`flex flex-col items-center justify-center gap-1 rounded-lg border-2 border-dashed p-6 text-center transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring ${
            dragging ? 'border-primary bg-primary/5' : 'border-border'
          }`}
          onDragOver={(e) => {
            e.preventDefault();
            setDragging(true);
          }}
          onDragLeave={() => setDragging(false)}
          onDrop={(e) => {
            e.preventDefault();
            setDragging(false);
            handleFiles(e.dataTransfer.files);
          }}
          onClick={() => {
            if (!disabled) fileInputRef.current?.click();
          }}
          role="button"
        >
          {isUploading ? (
            <Loader2 className="h-6 w-6 animate-spin text-muted-foreground" />
          ) : (
            <FileUp className="h-6 w-6 text-muted-foreground" />
          )}
          <span className="text-sm font-medium">{labels.uploadHint}</span>
          <span className="text-xs text-muted-foreground">{labels.browse}</span>
        </div>
        {pending.length > 0 && (
          <div className="mt-3 space-y-1">
            {pending.map((p) => (
              <div key={p.id} className="flex items-center gap-2 text-xs">
                {p.status === 'uploading' ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin text-muted-foreground" />
                ) : (
                  <span className="h-3.5 w-3.5 shrink-0 text-destructive">!</span>
                )}
                {p.status === 'uploading' ? (
                  <>
                    <span className="text-muted-foreground">{p.name}</span>
                    <span className="text-muted-foreground">{t('ks.uploading')}</span>
                  </>
                ) : (
                  <span className="text-destructive break-words">{p.reason ?? p.name}</span>
                )}
              </div>
            ))}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

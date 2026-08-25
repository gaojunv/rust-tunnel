import { useRef, useState } from 'react';
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

export const ACCEPT_STRING = ACCEPTED_EXTENSIONS.map((e) => `.${e}`).join(',');

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
  const [dragging, setDragging] = useState(false);
  const [localError, setLocalError] = useState<string | null>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);

  const handleFiles = (list: FileList | null) => {
    if (!list || list.length === 0) return;
    let hasInvalid = false;
    const accepted: File[] = [];
    Array.from(list).forEach((f) => {
      const ext = f.name.toLowerCase().split('.').pop() ?? '';
      if (ACCEPTED_EXTENSIONS.includes(ext) && f.size <= maxBytesFor(ext)) {
        accepted.push(f);
      } else {
        hasInvalid = true;
      }
    });
    setLocalError(hasInvalid ? labels.fileInvalid : null);
    accepted.forEach((f) => {
      try {
        const result = onUpload(f);
        if (result instanceof Promise) {
          result.catch((err: unknown) => {
            if (formatUploadError) setLocalError(formatUploadError(err));
          });
        }
      } catch (err: unknown) {
        if (formatUploadError) setLocalError(formatUploadError(err));
      }
    });
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
          className={`flex flex-col items-center justify-center gap-1 rounded-lg border-2 border-dashed p-6 text-center transition-colors ${
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
          {localError && <span className="text-xs text-destructive">{localError}</span>}
        </div>
      </CardContent>
    </Card>
  );
}

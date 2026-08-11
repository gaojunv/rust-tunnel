import { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery } from '@tanstack/react-query';
import { listWorkspaceFiles } from '@/api/client';

interface Props {
  workspaceId: string;
  query: string; // @ 后已输入的前缀
  /** 父组件键盘驱动的高亮下标（受控组件：↑↓ 由 textarea onKeyDown 转发）。 */
  activeIdx: number;
  /** 列表变化时把高亮回卷到首项（父组件持有 activeIdx，经此回调重置）。 */
  onActiveIdxChange: (idx: number) => void;
  /** 把可选中文件列表上报给父组件（Enter/Tab 选中依赖）。 */
  onFilesChange: (files: string[]) => void;
  onSelect: (path: string) => void;
}

export default function MentionPopup({
  workspaceId,
  query,
  activeIdx,
  onActiveIdxChange,
  onFilesChange,
  onSelect,
}: Props) {
  const { t } = useTranslation();
  const [debouncedQ, setDebouncedQ] = useState(query);

  useEffect(() => {
    const timer = setTimeout(() => setDebouncedQ(query), 300);
    return () => clearTimeout(timer);
  }, [query]);

  const { data } = useQuery({
    queryKey: ['agent-files', workspaceId, debouncedQ],
    queryFn: () => listWorkspaceFiles(workspaceId, debouncedQ),
    staleTime: 30_000,
  });
  // useMemo 固定引用：data 未就绪时不再每渲染新建 [] 字面量，effect 依赖才稳定
  const files = useMemo(() => data?.files ?? [], [data]);

  // 内容守卫：仅当列表内容实际变化时上报并重置高亮。files 引用（data?.files ?? []
  // 的字面量）在 data 未就绪时每渲染都新建，若不守卫会与父组件 setState 互相
  // 触发渲染循环。
  const filesRef = useRef<string[] | null>(null);
  useEffect(() => {
    const prev = filesRef.current;
    if (
      prev !== null &&
      prev.length === files.length &&
      prev.every((f, i) => f === files[i])
    ) {
      return;
    }
    filesRef.current = files;
    onFilesChange(files);
    onActiveIdxChange(0);
  }, [files, onFilesChange, onActiveIdxChange]);

  return (
    <div className="absolute bottom-full left-0 mb-1 max-h-56 w-80 max-w-full overflow-y-auto rounded-lg border bg-popover shadow-lg">
      {files.length === 0 && (
        <p className="px-3 py-2 text-xs text-muted-foreground">{t('agent.noMatchingFiles')}</p>
      )}
      {files.map((f, i) => (
        <button
          key={f}
          type="button"
          onClick={() => onSelect(f)}
          className={`block w-full truncate px-3 py-1.5 text-left text-sm ${i === activeIdx ? 'bg-accent' : ''}`}
        >
          {f}
        </button>
      ))}
    </div>
  );
}

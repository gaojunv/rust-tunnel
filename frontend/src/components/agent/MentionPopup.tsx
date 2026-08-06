import { useEffect, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { listWorkspaceFiles } from '@/api/client';

interface Props {
  workspaceId: string;
  query: string; // @ 后已输入的前缀
  onSelect: (path: string) => void;
  onClose: () => void;
}

export default function MentionPopup({ workspaceId, query, onSelect }: Props) {
  const [debouncedQ, setDebouncedQ] = useState(query);
  const [activeIdx, setActiveIdx] = useState(0);

  useEffect(() => {
    const timer = setTimeout(() => setDebouncedQ(query), 300);
    return () => clearTimeout(timer);
  }, [query]);

  const { data } = useQuery({
    queryKey: ['agent-files', workspaceId, debouncedQ],
    queryFn: () => listWorkspaceFiles(workspaceId, debouncedQ),
    staleTime: 30_000,
  });
  const files = data?.files ?? [];

  // 键盘事件由父组件转发（见 ChatStream 的 textarea onKeyDown）
  useEffect(() => { setActiveIdx(0); }, [files.length]);

  return (
    <div className="absolute bottom-full left-0 mb-1 max-h-56 w-80 overflow-y-auto rounded-lg border bg-popover shadow-lg">
      {files.length === 0 && (
        <p className="px-3 py-2 text-xs text-muted-foreground">无匹配文件</p>
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

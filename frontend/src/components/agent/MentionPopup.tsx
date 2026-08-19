import { useEffect, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery } from '@tanstack/react-query';
import { Bot, File } from 'lucide-react';
import { listWorkspaceFiles } from '@/api/client';
import type { AgentRole } from '@/types';

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
  /** 启用的角色列表（用于 @ 补全候选；空数组=不显示角色）。 */
  roles?: AgentRole[];
}

export type MentionCandidate =
  | { kind: 'file'; value: string }
  | { kind: 'role'; value: string; role: AgentRole };

export function filterMentionCandidates(
  query: string,
  files: string[],
  roles: AgentRole[],
): MentionCandidate[] {
  const q = query.toLowerCase();
  const roleMatches = roles
    .filter((r) => r.enabled && r.name.toLowerCase().includes(q))
    .map<MentionCandidate>((r) => ({ kind: 'role', value: r.name, role: r }));
  const fileMatches = files
    .filter((f) => f.toLowerCase().includes(q))
    .map<MentionCandidate>((f) => ({ kind: 'file', value: f }));
  return [...roleMatches, ...fileMatches];
}

export default function MentionPopup({
  workspaceId,
  query,
  activeIdx,
  onActiveIdxChange,
  onFilesChange,
  onSelect,
  roles = [],
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

  const candidates = useMemo(
    () => filterMentionCandidates(debouncedQ, files, roles),
    [debouncedQ, files, roles],
  );

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

  if (candidates.length === 0) {
    return (
      <div className="absolute bottom-full left-0 mb-1 max-h-56 w-80 max-w-full overflow-y-auto rounded-lg border bg-popover shadow-lg">
        <p className="px-3 py-2 text-xs text-muted-foreground">{t('agent.noMatchingFiles')}</p>
      </div>
    );
  }

  return (
    <div className="absolute bottom-full left-0 mb-1 max-h-56 w-80 max-w-full overflow-y-auto rounded-lg border bg-popover shadow-lg">
      {candidates.map((c, i) => (
        <button
          key={c.kind === 'role' ? `role-${c.value}` : c.value}
          type="button"
          onClick={() => onSelect(c.kind === 'role' ? `@${c.value}` : c.value)}
          className={`flex w-full items-center gap-2 truncate px-3 py-1.5 text-left text-sm ${i === activeIdx ? 'bg-accent' : ''}`}
        >
          {c.kind === 'role' ? (
            <Bot className="h-3.5 w-3.5 shrink-0 text-violet-500" />
          ) : (
            <File className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
          )}
          {c.kind === 'role' ? (
            <>
              <span className="truncate font-medium">{c.value}</span>
              <span className="ml-auto shrink-0 text-[10px] text-muted-foreground">
                {t('role.candidateRole')}
              </span>
            </>
          ) : (
            <span className="truncate">{c.value}</span>
          )}
        </button>
      ))}
    </div>
  );
}

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useReducer,
  useRef,
  useState,
} from 'react';
import { useTranslation } from 'react-i18next';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Tree, type NodeRendererProps } from 'react-arborist';
import {
  ArrowLeft,
  ChevronDown,
  ChevronRight,
  FileDiff,
  FileText,
  Folder,
  Pencil,
  RefreshCw,
  Save,
  X,
} from 'lucide-react';
import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../../ui/dialog';
import {
  getAgentGitStatus,
  getApiErrorMessage,
  getFsFile,
  getFsTree,
  putFsFile,
} from '../../../api/client';
import type { FsEntry, FsFileContent, GitStatusResult } from '../../../types';
import CodeMirrorEditor, { isEditorSupported } from '../CodeMirrorEditor';
import FileDiffView from './FileDiffView';
import {
  clearDraft,
  closePath,
  isDirty,
  loadOpenFiles,
  onDraftsChanged,
  openOrActivate,
  readDraft,
  saveOpenFiles,
  writeDraft,
  type FileTabsState,
} from '../fileTabsStore';

/** 懒加载文件树的受控数据节点。id 使用相对工作区根目录的完整路径（与 git status 路径一致）。 */
interface FsNode {
  id: string;
  name: string;
  isDir: boolean;
  children?: FsNode[];
}

/**
 * 解析 `git status --porcelain=v1 -b` 原文为 路径→归一状态 的映射。
 * - 跳过 `## ` 分支头行
 * - `?? path` → untracked（值 '??'）
 * - `XY path` → 优先取 index 状态 X，其次取 worktree 状态 Y（'M'/'A'/'D'/'R' 等）
 * - 重命名行 `R  old -> new` 只记录新路径
 */
export function parsePorcelain(status: string): Map<string, string> {
  const map = new Map<string, string>();
  for (const raw of status.split('\n')) {
    const line = raw.endsWith('\r') ? raw.slice(0, -1) : raw;
    if (line === '' || line.startsWith('## ')) continue;

    if (line.startsWith('?? ')) {
      map.set(line.slice(3), '??');
      continue;
    }
    // porcelain 行最小形态为 "XY path"（至少 4 字符）
    if (line.length < 4) continue;
    const x = line[0];
    const y = line[1];
    let path = line.slice(3);
    // 重命名：`R  old -> new`（仅重命名行才按 ` -> ` 拆分，避免误伤普通路径）
    if ((x === 'R' || y === 'R') && path.includes(' -> ')) {
      path = path.slice(path.lastIndexOf(' -> ') + 4);
    }
    map.set(path, x !== ' ' && x !== '?' ? x : y);
  }
  return map;
}

/** 目录在前、按名称排序，过滤 `.git`。 */
function sortEntries(entries: FsEntry[]): FsEntry[] {
  return [...entries]
    .filter((e) => e.name !== '.git')
    .sort((a, b) => {
      if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1;
      return a.name.localeCompare(b.name);
    });
}

function findNode(nodes: FsNode[], id: string): FsNode | undefined {
  for (const n of nodes) {
    if (n.id === id) return n;
    if (n.children) {
      const found = findNode(n.children, id);
      if (found) return found;
    }
  }
  return undefined;
}

/** 不可变地把 id 节点的 children 替换为加载结果。 */
function updateNodeChildren(nodes: FsNode[], id: string, children: FsNode[]): FsNode[] {
  return nodes.map((n) => {
    if (n.id === id) return { ...n, children };
    if (n.children) return { ...n, children: updateNodeChildren(n.children, id, children) };
    return n;
  });
}

// ── git 状态标注配色（VS Code 风格字母徽章）──────────────────────

function statusColor(status: string): string {
  if (status === 'M' || status === 'R') return 'text-yellow-500 dark:text-yellow-400';
  if (status === 'A' || status === 'C' || status === '??') {
    return 'text-green-500 dark:text-green-400';
  }
  if (status === 'D') return 'text-red-500 dark:text-red-400';
  return 'text-muted-foreground';
}

function badgeClass(status: string): string {
  if (status === 'M' || status === 'R') {
    return 'bg-yellow-500/15 text-yellow-600 dark:text-yellow-400';
  }
  if (status === 'A' || status === 'C' || status === '??') {
    return 'bg-green-500/15 text-green-600 dark:text-green-400';
  }
  if (status === 'D') return 'bg-red-500/15 text-red-600 dark:text-red-400';
  return 'bg-gray-500/15 text-gray-500 dark:text-gray-400';
}

function badgeLetter(status: string): string {
  return status === '??' ? 'U' : status;
}

/** 目录自身无状态，但存在子路径变更时按首个子项状态着色。
 *  预聚合（O(dirs + git entries)，替代原 O(dirs × entries) 逐目录全量扫描）：
 *  单次遍历 gitMap，把每个 git 条目的路径前缀逐级归入对应目录（`a/b/c.ts` →
 *  目录 `a`、`a/b`），首个命中者优先——与旧 `dirStatus` 的「Map 迭代序首项」
 *  语义一致。之后 TreeNode 对任意目录 O(1) 查表。 */
function buildDirStatus(map: Map<string, string>): Map<string, string> {
  const dirStatus = new Map<string, string>();
  for (const [path, status] of map) {
    let slash = path.indexOf('/');
    while (slash !== -1) {
      const dir = path.slice(0, slash);
      if (!dirStatus.has(dir)) dirStatus.set(dir, status);
      slash = path.indexOf('/', slash + 1);
    }
  }
  return dirStatus;
}

// ── 容器尺寸测量（react-window 需要确定的高宽） ─────────────────
// 用 callback ref + 元素状态：树视图卸载（切到预览）后再挂载时会重新测量，
// 而不是只在首次挂载时测量一次。

function useElementSize(): [React.RefCallback<HTMLDivElement>, { width: number; height: number }] {
  // jsdom/未挂载时无 clientWidth/Height → 回退默认值，保证 react-window 仍渲染可见行
  const [size, setSize] = useState({ width: 300, height: 500 });
  const [el, setEl] = useState<HTMLDivElement | null>(null);
  const ref = useCallback((node: HTMLDivElement | null) => setEl(node), []);
  useEffect(() => {
    if (!el) return;
    const measure = () => {
      setSize({ width: el.clientWidth || 300, height: el.clientHeight || 500 });
    };
    measure();
    if (typeof ResizeObserver !== 'undefined') {
      const ro = new ResizeObserver(measure);
      ro.observe(el);
      return () => ro.disconnect();
    }
    window.addEventListener('resize', measure);
    return () => window.removeEventListener('resize', measure);
  }, [el]);
  return [ref, size];
}

/** 跟随页面明暗主题（theme 由 ThemeProvider 切换 documentElement.dark class）。 */
function useIsDark(): boolean {
  const [isDark, setIsDark] = useState(
    () => typeof document !== 'undefined' && document.documentElement.classList.contains('dark')
  );
  useEffect(() => {
    if (typeof document === 'undefined') return;
    const el = document.documentElement;
    const update = () => setIsDark(el.classList.contains('dark'));
    update();
    const mo = new MutationObserver(update);
    mo.observe(el, { attributes: true, attributeFilter: ['class'] });
    return () => mo.disconnect();
  }, []);
  return isDark;
}

// ── 树节点渲染 ───────────────────────────────────────────────

interface NodeViewContextValue {
  gitMap: Map<string, string>;
  /** 目录路径 → 该目录下首个 git 变更子项的状态（预聚合，O(1) 查表）。 */
  dirStatus: Map<string, string>;
  onOpenFile: (path: string) => void;
}

const NodeViewContext = createContext<NodeViewContextValue>({
  gitMap: new Map(),
  dirStatus: new Map(),
  onOpenFile: () => {},
});

function TreeNode({ node, style }: NodeRendererProps<FsNode>) {
  const { gitMap, dirStatus, onOpenFile } = useContext(NodeViewContext);
  const isDir = node.data.isDir;
  const path = node.id;
  const status = isDir ? (dirStatus.get(path) ?? null) : (gitMap.get(path) ?? null);

  return (
    <div
      style={style}
      onClick={(e) => {
        node.handleClick(e);
        if (!isDir) onOpenFile(path);
      }}
      className={cn(
        'flex w-full cursor-pointer items-center gap-1.5 rounded-sm py-0.5 pr-2 text-xs',
        node.isSelected ? 'bg-accent text-foreground' : 'hover:bg-accent/50'
      )}
    >
      {isDir ? (
        <button
          type="button"
          aria-label={node.isOpen ? 'collapse' : 'expand'}
          onClick={(e) => {
            e.stopPropagation();
            node.toggle();
          }}
          className="flex h-4 w-4 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-accent"
        >
          {node.isOpen ? (
            <ChevronDown className="h-3.5 w-3.5" />
          ) : (
            <ChevronRight className="h-3.5 w-3.5" />
          )}
        </button>
      ) : (
        <span className="w-4 shrink-0" />
      )}
      {isDir ? (
        <Folder className={cn('h-3.5 w-3.5 shrink-0', status ? statusColor(status) : 'text-sky-500')} />
      ) : (
        <FileText className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
      )}
      <span className={cn('min-w-0 flex-1 truncate', status ? statusColor(status) : '')}>
        {node.data.name}
      </span>
      {status && (
        <span
          className={cn(
            'inline-flex h-4 w-4 shrink-0 items-center justify-center rounded-sm font-mono text-[10px] font-bold',
            badgeClass(status)
          )}
        >
          {badgeLetter(status)}
        </span>
      )}
    </div>
  );
}

// ── 文件预览 / 编辑（多标签中的一个文件） ─────────────────────

function FileView({
  workspaceId,
  path,
  isActive,
  onClose,
  isDark,
}: {
  workspaceId: string;
  path: string;
  isActive: boolean;
  onClose: (path: string) => void;
  isDark: boolean;
}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();

  const [editing, setEditing] = useState(false);
  const [diffMode, setDiffMode] = useState(false);
  // 草稿恢复：store 有上次未保存内容则用之，否则等远端内容首次到达时 seed
  const [draft, setDraft] = useState<string>(() => readDraft(workspaceId, path)?.draft ?? '');
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const seededRef = useRef(false);

  const fileQuery = useQuery<FsFileContent>({
    queryKey: ['agent-fs-file', workspaceId, path],
    queryFn: () => getFsFile(workspaceId, path),
    retry: false,
  });

  const content = fileQuery.data?.content ?? '';

  // 首次数据到达且 store 无草稿时，以远端内容初始化（seed 一次；保存后刷新不覆盖草稿）
  useEffect(() => {
    if (fileQuery.data && !seededRef.current) {
      seededRef.current = true;
      if (readDraft(workspaceId, path) == null) {
        setDraft(fileQuery.data.content);
      }
    }
  }, [fileQuery.data, workspaceId, path]);

  // 草稿同步：内容偏离已保存版本 → 写入 store（未保存圆点）；回到已保存 → 清除
  useEffect(() => {
    if (draft !== content) {
      writeDraft(workspaceId, path, draft);
    } else {
      clearDraft(workspaceId, path);
    }
  }, [draft, content, workspaceId, path]);

  const saveMutation = useMutation({
    mutationFn: (approved: boolean) => putFsFile(workspaceId, path, draft, approved),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['agent-fs-file', workspaceId, path] });
      queryClient.invalidateQueries({ queryKey: ['agent-git-status', workspaceId] });
      clearDraft(workspaceId, path);
      setEditing(false);
      setConfirmOpen(false);
      setSaveError(null);
    },
    onError: (err: unknown) => {
      const status = (err as { response?: { status?: number } })?.response?.status;
      if (status === 409) {
        // 后端审批未确认 → 弹窗确认后带 approved=true 重发
        setConfirmOpen(true);
      } else {
        setSaveError(getApiErrorMessage(err));
      }
    },
  });

  const startEditing = () => {
    // 草稿已持久化在 store：保留上次未保存内容，不重置为远端快照
    setSaveError(null);
    setEditing(true);
  };
  const cancelEditing = () => {
    setEditing(false);
    setSaveError(null);
  };
  const handleSave = () => {
    setSaveError(null);
    saveMutation.mutate(false);
  };
  const handleConfirmSave = () => {
    setConfirmOpen(false);
    saveMutation.mutate(true);
  };

  return (
    <div className="flex h-full min-h-0 flex-col" aria-hidden={!isActive}>
      {/* 顶栏：返回(关闭当前标签) + 路径 + 操作按钮 */}
      <div className="flex items-center gap-1 border-b border-border/60 px-1 pb-1.5">
        <Button
          variant="ghost"
          size="sm"
          onClick={() => onClose(path)}
          aria-label={t('agent.backToTree')}
          title={t('agent.backToTree')}
          className="h-6 shrink-0 px-1.5"
        >
          <ArrowLeft className="h-3.5 w-3.5" />
        </Button>
        <span className="min-w-0 flex-1 truncate font-mono text-xs text-foreground" title={path}>
          {path}
        </span>
        {!editing ? (
          <Button
            variant="ghost"
            size="sm"
            onClick={startEditing}
            // 截断文件（>100KB）禁止编辑：草稿只有部分内容，保存会截断远端文件
            disabled={!fileQuery.isSuccess || fileQuery.data?.truncated || saveMutation.isPending}
            aria-label={t('agent.edit')}
            className="h-6 shrink-0 gap-1 px-2 text-xs"
          >
            <Pencil className="h-3 w-3" />
            {t('agent.edit')}
          </Button>
        ) : (
          <>
            <Button
              variant={diffMode ? 'default' : 'ghost'}
              size="sm"
              onClick={() => setDiffMode((v) => !v)}
              aria-pressed={diffMode}
              aria-label={t('agent.compare')}
              title={t('agent.compareSaved')}
              className="h-6 shrink-0 gap-1 px-2 text-xs"
            >
              <FileDiff className="h-3 w-3" />
              {t('agent.compare')}
            </Button>
            <Button
              variant="default"
              size="sm"
              onClick={handleSave}
              disabled={saveMutation.isPending}
              aria-label={t('agent.save')}
              className="h-6 shrink-0 gap-1 px-2 text-xs"
            >
              <Save className="h-3 w-3" />
              {t('agent.save')}
            </Button>
            <Button
              variant="ghost"
              size="sm"
              onClick={cancelEditing}
              disabled={saveMutation.isPending}
              aria-label={t('agent.cancel')}
              className="h-6 shrink-0 gap-1 px-2 text-xs"
            >
              <X className="h-3 w-3" />
              {t('agent.cancel')}
            </Button>
          </>
        )}
      </div>

      {fileQuery.isLoading ? (
        <div className="flex flex-1 items-center justify-center p-4 text-xs text-muted-foreground">
          {t('common.loading')}
        </div>
      ) : fileQuery.isError ? (
        <div className="flex flex-1 items-center justify-center p-4 text-center text-xs text-muted-foreground">
          {t('agent.clientOffline')}
        </div>
      ) : editing ? (
        diffMode ? (
          // 并排对比：左=已保存（只读），右=草稿（可编辑）；jsdom 下走 fallback pre
          <FileDiffView
            saved={content}
            draft={draft}
            onDraftChange={setDraft}
            path={path}
            isDark={isDark}
          />
        ) : isEditorSupported() ? (
          <CodeMirrorEditor value={draft} onChange={setDraft} path={path} isDark={isDark} />
        ) : (
          <textarea
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            spellCheck={false}
            data-testid="file-editor"
            className="min-h-0 flex-1 resize-none bg-transparent p-2 font-mono text-xs leading-5 outline-none"
          />
        )
      ) : (
        <div className="min-h-0 flex-1 overflow-hidden" data-testid="file-preview">
          {fileQuery.data?.truncated && (
            <div className="border-b border-border/60 bg-muted/80 px-2 py-1 text-[11px] text-muted-foreground">
              {t('agent.fileTruncated')}
            </div>
          )}
          {/* 只读 CodeMirror 预览（jsdom 退化纯文本）替代原 shiki HTML */}
          <CodeMirrorEditor readOnly value={content} path={path} isDark={isDark} />
        </div>
      )}

      {saveError && (
        <p className="px-2 py-1 text-xs text-destructive" role="alert">
          {t('agent.saveFailed')}
        </p>
      )}

      <Dialog open={confirmOpen} onOpenChange={setConfirmOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{t('agent.confirmSaveTitle')}</DialogTitle>
            <DialogDescription>{t('agent.confirmSaveDesc')}</DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="ghost" size="sm" onClick={() => setConfirmOpen(false)}>
              {t('agent.cancel')}
            </Button>
            <Button variant="default" size="sm" onClick={handleConfirmSave}>
              {t('agent.confirm')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

// ── 面板主组件 ───────────────────────────────────────────────

export default function FilesPanel({ workspaceId }: { workspaceId: string }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const isDark = useIsDark();

  const [data, setData] = useState<FsNode[]>([]);
  const [fileTabs, setFileTabs] = useState<FileTabsState>(
    () => loadOpenFiles(workspaceId) ?? { open: [], active: '' }
  );
  const [treeKey, setTreeKey] = useState(0);
  const loadingRef = useRef(new Set<string>());
  const [sizeRef, size] = useElementSize();
  // 草稿变化订阅 → 刷新标签条「● 未保存」圆点（store 不是响应式，靠事件驱动重渲染）
  const [, forceUpdate] = useReducer((x: number) => x + 1, 0);
  useEffect(() => onDraftsChanged(forceUpdate), [forceUpdate]);

  const rootQuery = useQuery<FsEntry[]>({
    queryKey: ['agent-fs', workspaceId, ''],
    queryFn: () => getFsTree(workspaceId),
    enabled: !!workspaceId,
    retry: false,
  });

  // 根目录数据到达/刷新后重建树（展开过的子目录重置，刷新=整树重载）
  useEffect(() => {
    if (rootQuery.data) {
      setData(
        sortEntries(rootQuery.data).map((e) => ({ id: e.name, name: e.name, isDir: e.is_dir }))
      );
    }
  }, [rootQuery.data]);

  // 切 workspace 时重置打开的标签（修复残留旧 workspace 路径的隐患）
  useEffect(() => {
    setFileTabs(loadOpenFiles(workspaceId) ?? { open: [], active: '' });
  }, [workspaceId]);

  // 标签变更统一入口：更新 state 并持久化。在 functional updater 内写 localStorage
  // 虽非纯函数惯例，但 safeStorage 写失败静默、重复执行幂等，换取「不用 effect
  // 追踪持久化」从而避免 workspace 切换瞬间把旧标签误写进新 key。
  const updateTabs = useCallback(
    (updater: (s: FileTabsState) => FileTabsState) => {
      setFileTabs((prev) => {
        const next = updater(prev);
        saveOpenFiles(workspaceId, next);
        return next;
      });
    },
    [workspaceId]
  );

  const gitQuery = useQuery<GitStatusResult>({
    queryKey: ['agent-git-status', workspaceId],
    queryFn: () => getAgentGitStatus(workspaceId),
    enabled: !!workspaceId,
    retry: false,
  });
  const gitMap = useMemo(
    () => (gitQuery.data ? parsePorcelain(gitQuery.data.status) : new Map<string, string>()),
    [gitQuery.data]
  );
  const contextValue = useMemo(
    () => ({
      gitMap,
      dirStatus: buildDirStatus(gitMap),
      onOpenFile: (path: string) => updateTabs((s) => openOrActivate(s, path)),
    }),
    [gitMap, updateTabs]
  );

  const handleToggle = async (id: string) => {
    if (loadingRef.current.has(id)) return;
    const node = findNode(data, id);
    if (!node || !node.isDir || node.children !== undefined) return;
    loadingRef.current.add(id);
    try {
      const entries = await getFsTree(workspaceId, id);
      const children = sortEntries(entries).map((e) => ({
        id: `${id}/${e.name}`,
        name: e.name,
        isDir: e.is_dir,
      }));
      setData((prev) => updateNodeChildren(prev, id, children));
    } catch {
      /* 展开加载失败：保持空目录，等待刷新重试 */
    } finally {
      loadingRef.current.delete(id);
    }
  };

  const refresh = () => {
    queryClient.invalidateQueries({ queryKey: ['agent-fs'] });
    queryClient.invalidateQueries({ queryKey: ['agent-git-status', workspaceId] });
    setTreeKey((k) => k + 1);
  };

  if (!workspaceId) return null;

  return (
    <div className="flex h-full min-h-0 flex-col">
      {/* 工具行：标题 + 刷新 */}
      <div className="flex items-center justify-between px-1 pb-1 pt-0.5">
        <span className="text-[10px] font-semibold uppercase tracking-wide text-muted-foreground/70">
          {t('agent.files')}
        </span>
        <button
          type="button"
          aria-label={t('agent.refresh')}
          title={t('agent.refresh')}
          onClick={refresh}
          className="rounded p-1 text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
        >
          <RefreshCw className={cn('h-3.5 w-3.5', rootQuery.isFetching && 'animate-spin')} />
        </button>
      </div>

      {/* 文件标签条：多文件打开时显示；未保存前置圆点（amber）；激活高亮 */}
      {fileTabs.open.length > 0 && (
        <div
          role="tablist"
          aria-label={t('agent.files')}
          className="mb-1 flex items-center gap-0.5 overflow-x-auto px-1"
        >
          {fileTabs.open.map((path) => {
            const active = path === fileTabs.active;
            const dirty = isDirty(workspaceId, path);
            const base = path.split('/').pop() || path;
            return (
              <div
                key={path}
                role="tab"
                aria-selected={active}
                title={dirty ? `${t('agent.unsavedChanges')} · ${path}` : path}
                onClick={() => updateTabs((s) => ({ ...s, active: path }))}
                className={cn(
                  'group flex max-w-[10rem] shrink-0 cursor-pointer items-center gap-1 rounded border border-border/60 px-1.5 py-0.5 text-[11px] font-mono',
                  active
                    ? 'border-accent bg-accent text-foreground'
                    : 'text-muted-foreground hover:bg-accent/50 hover:text-foreground'
                )}
              >
                {dirty && (
                  <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-amber-500" aria-hidden />
                )}
                <span className="min-w-0 flex-1 truncate">{base}</span>
                <button
                  type="button"
                  aria-label={t('agent.closeFile')}
                  title={t('agent.closeFile')}
                  onClick={(e) => {
                    e.stopPropagation();
                    updateTabs((s) => closePath(s, path));
                  }}
                  className="shrink-0 rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground"
                >
                  <X className="h-3 w-3" />
                </button>
              </div>
            );
          })}
        </div>
      )}

      {fileTabs.open.length > 0 ? (
        /* 打开文件视图：所有标签常驻挂载，仅激活者可见（切标签不丢草稿状态）。
           优先于树错误分支：已打开的文件在树加载失败时仍可访问，与单文件时代的语义一致。 */
        <div className="relative min-h-0 flex-1">
          {fileTabs.open.map((path) => (
            <div
              key={path}
              data-testid={`file-tab-view-${path}`}
              className={path === fileTabs.active ? 'h-full min-h-0' : 'hidden'}
            >
              <FileView
                key={`${workspaceId}:${path}`}
                workspaceId={workspaceId}
                path={path}
                isActive={path === fileTabs.active}
                isDark={isDark}
                onClose={(p) => updateTabs((s) => closePath(s, p))}
              />
            </div>
          ))}
        </div>
      ) : rootQuery.isError ? (
        <div className="flex flex-1 items-center justify-center p-4 text-center text-xs text-muted-foreground">
          {t('agent.clientOffline')}
        </div>
      ) : (
        <>
          {rootQuery.isSuccess && data.length > 0 && (
            <p className="px-1 pb-1 text-[10px] text-muted-foreground/60">
              {t('agent.noFileSelected')}
            </p>
          )}
          <NodeViewContext.Provider value={contextValue}>
            <div ref={sizeRef} className="min-h-0 flex-1">
              {rootQuery.isSuccess && data.length === 0 ? (
                <div className="flex h-full items-center justify-center p-4 text-xs text-muted-foreground">
                  {t('agent.emptyDir')}
                </div>
              ) : (
                <Tree<FsNode>
                  key={treeKey}
                  data={data}
                  idAccessor={(d) => d.id}
                  childrenAccessor={(d) => (d.isDir ? (d.children ?? []) : null)}
                  rowHeight={24}
                  indent={14}
                  openByDefault={false}
                  disableMultiSelection
                  disableDrag
                  disableDrop
                  disableEdit
                  onToggle={handleToggle}
                  width={size.width}
                  height={size.height}
                  className="h-full"
                  aria-label={t('agent.files')}
                >
                  {TreeNode}
                </Tree>
              )}
            </div>
          </NodeViewContext.Provider>
        </>
      )}
    </div>
  );
}
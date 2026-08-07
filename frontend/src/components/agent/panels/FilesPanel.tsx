import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react';
import { useTranslation } from 'react-i18next';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Tree, type NodeRendererProps } from 'react-arborist';
import { codeToHtml, type BundledLanguage } from 'shiki';
import {
  ArrowLeft,
  ChevronDown,
  ChevronRight,
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

/** 目录自身无状态，但存在子路径变更时按首个子项状态着色。 */
function dirStatus(map: Map<string, string>, dirPath: string): string | null {
  const prefix = `${dirPath}/`;
  for (const [path, status] of map) {
    if (path.startsWith(prefix)) return status;
  }
  return null;
}

// ── shiki 语言映射 ─────────────────────────────────────────────

const LANG_BY_EXT: Record<string, BundledLanguage> = {
  rs: 'rust',
  ts: 'typescript',
  tsx: 'tsx',
  js: 'javascript',
  jsx: 'jsx',
  py: 'python',
  go: 'go',
  md: 'markdown',
  json: 'json',
  toml: 'toml',
  yaml: 'yaml',
  yml: 'yaml',
  sh: 'bash',
  css: 'css',
  html: 'html',
  vue: 'vue',
  java: 'java',
  c: 'c',
  cpp: 'cpp',
  h: 'c',
  sql: 'sql',
  xml: 'xml',
};

function languageForPath(path: string): BundledLanguage {
  const ext = path.split('.').pop()?.toLowerCase() ?? '';
  // 'text' 是 shiki 的内置特殊语言（无语法高亮），但不在 BundledLanguage 联合类型里
  return LANG_BY_EXT[ext] ?? ('text' as BundledLanguage);
}

/** 轻量内容指纹：shiki 查询 key 里放全文太长，用 长度+哈希 足够区分内容变化。 */
function contentHash(content: string): string {
  let h = 5381;
  for (let i = 0; i < content.length; i++) {
    h = ((h << 5) + h + content.charCodeAt(i)) | 0;
  }
  return `${content.length}:${h.toString(36)}`;
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
  onOpenFile: (path: string) => void;
}

const NodeViewContext = createContext<NodeViewContextValue>({
  gitMap: new Map(),
  onOpenFile: () => {},
});

function TreeNode({ node, style }: NodeRendererProps<FsNode>) {
  const { gitMap, onOpenFile } = useContext(NodeViewContext);
  const isDir = node.data.isDir;
  const path = node.id;
  const status = isDir ? dirStatus(gitMap, path) : (gitMap.get(path) ?? null);

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

// ── 文件预览 / 编辑 ───────────────────────────────────────────

function FileView({
  workspaceId,
  path,
  onBack,
}: {
  workspaceId: string;
  path: string;
  onBack: () => void;
}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const isDark = useIsDark();

  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState('');
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);

  const fileQuery = useQuery<FsFileContent>({
    queryKey: ['agent-fs-file', workspaceId, path],
    queryFn: () => getFsFile(workspaceId, path),
    retry: false,
  });

  const content = fileQuery.data?.content ?? '';

  const highlightQuery = useQuery({
    queryKey: ['shiki', path, contentHash(content), isDark],
    queryFn: () =>
      codeToHtml(content, {
        lang: languageForPath(path),
        theme: isDark ? 'dark-plus' : 'light-plus',
      }),
    enabled: fileQuery.isSuccess && !editing,
  });

  const saveMutation = useMutation({
    mutationFn: (approved: boolean) => putFsFile(workspaceId, path, draft, approved),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['agent-fs-file', workspaceId, path] });
      queryClient.invalidateQueries({ queryKey: ['agent-git-status', workspaceId] });
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
    setDraft(content);
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
    <div className="flex h-full min-h-0 flex-col">
      {/* 顶栏：返回 + 路径 + 操作按钮 */}
      <div className="flex items-center gap-1 border-b border-border/60 px-1 pb-1.5">
        <Button
          variant="ghost"
          size="sm"
          onClick={onBack}
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
        <textarea
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          spellCheck={false}
          data-testid="file-editor"
          className="min-h-0 flex-1 resize-none bg-transparent p-2 font-mono text-xs leading-5 outline-none"
        />
      ) : (
        <div className="min-h-0 flex-1 overflow-auto" data-testid="file-preview">
          {fileQuery.data?.truncated && (
            <div className="sticky top-0 z-10 border-b border-border/60 bg-muted/80 px-2 py-1 text-[11px] text-muted-foreground">
              {t('agent.fileTruncated')}
            </div>
          )}
          {highlightQuery.data ? (
            <div dangerouslySetInnerHTML={{ __html: highlightQuery.data }} />
          ) : (
            <pre className="whitespace-pre-wrap p-2 font-mono text-xs leading-5">{content}</pre>
          )}
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

  const [data, setData] = useState<FsNode[]>([]);
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const [treeKey, setTreeKey] = useState(0);
  const loadingRef = useRef(new Set<string>());
  const [sizeRef, size] = useElementSize();

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
  const contextValue = useMemo(() => ({ gitMap, onOpenFile: setSelectedPath }), [gitMap]);

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
  if (selectedPath) {
    return (
      <FileView
        key={selectedPath}
        workspaceId={workspaceId}
        path={selectedPath}
        onBack={() => setSelectedPath(null)}
      />
    );
  }

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

      {rootQuery.isError ? (
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

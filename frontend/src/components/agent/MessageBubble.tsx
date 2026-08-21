import { memo, useCallback, useEffect, useId, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { useTranslation } from 'react-i18next';
import {
  ArrowRightLeft,
  Brain,
  ChevronDown,
  ChevronUp,
  FileText,
  FileAudio,
  Globe,
  Image as ImageIcon,
  ListChecks,
  Loader2,
  Paperclip,
  Pencil,
  Search,
  TerminalSquare,
  Trash2,
  Wrench,
} from 'lucide-react';
import type { ChatItem, ToolKind, PlanEntryItem } from './types';
import Markdown from './Markdown';
import ToolDiffView from './ToolDiffView';

/** 折叠阈值：tool 参数/结果超过该行数时只显示前 3 行，可手动展开。 */
const COLLAPSE_LINE_THRESHOLD = 6;
const COLLAPSE_VISIBLE_LINES = 3;
/** 字符上限保护：文本超过该字符数即折叠（防超长单行/巨量文本直接全量渲染撑爆
 *  布局），未展开时仅显示前 COLLAPSE_MAX_CHARS 字符。 */
const COLLAPSE_MAX_CHARS = 8000;

function firstLines(text: string, n: number): string {
  const lines = text.split('\n');
  if (lines.length <= n) return text;
  return lines.slice(0, n).join('\n');
}

/** UTF-16 安全截断：slice 按 UTF-16 码元切分，落在代理对中间会产出孤立半字符
 *  （乱码）。若截断点恰为代理对高位，回退 1 码元保证断点完整（无需 grapheme 级
 *  处理）。 */
function truncateChars(text: string, max: number): string {
  if (text.length <= max) return text;
  const code = text.charCodeAt(max - 1);
  const end = code >= 0xd800 && code <= 0xdbff ? max - 1 : max;
  return text.slice(0, end);
}

/** 工具调用的长文本（args/result）：超过阈值折叠为前 3 行 + 展开按钮。
 *  导出供 SubagentTaskCard 复用（父 Task 卡最终结果展示）。 */
export function CollapsiblePre({ text, className }: { text: string; className?: string }) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  const lineCount = text.split('\n').length;
  const charCount = text.length;
  const lineCollapsed = lineCount > COLLAPSE_LINE_THRESHOLD;
  const charCollapsed = charCount > COLLAPSE_MAX_CHARS;
  const collapsible = lineCollapsed || charCollapsed;
  const collapsed = collapsible && !expanded;
  // 折叠态内容：字符超限优先——显示前 MAX_CHARS 字符 + 省略标记（超长单行也
  // 会被截断）；仅行超限时维持「前 3 行」折叠。展开后一律显示完整文本（用户
  // 主动承担大文本渲染）。两者都未超限则全量显示、无按钮。
  const shown = collapsed
    ? charCollapsed
      ? `${truncateChars(text, COLLAPSE_MAX_CHARS)}…`
      : firstLines(text, COLLAPSE_VISIBLE_LINES)
    : text;

  return (
    <div className={className}>
      <pre className="whitespace-pre-wrap break-words text-xs text-muted-foreground">{shown}</pre>
      {collapsible && (
        <button
          type="button"
          onClick={() => setExpanded((v) => !v)}
          className="mt-0.5 flex items-center gap-0.5 text-xs text-primary hover:underline"
        >
          {expanded ? (
            <>
              <ChevronUp className="h-3 w-3" />
              {t('agent.collapse')}
            </>
          ) : charCollapsed ? (
            <>
              <ChevronDown className="h-3 w-3" />
              {t('agent.expandChars', { count: charCount })}
            </>
          ) : (
            <>
              <ChevronDown className="h-3 w-3" />
              {t('agent.expandLines', { count: lineCount - COLLAPSE_VISIBLE_LINES })}
            </>
          )}
        </button>
      )}
    </div>
  );
}

/** args 是否为「无信息的空 JSON」（`{}` / `null` / 空字符串）——展开时不应显示。 */
function isNoopArgs(args: string): boolean {
  const t = args.trim();
  if (!t) return true;
  if (t === '{}' || t === 'null' || t === '[]') return true;
  return false;
}

/** 取路径最后一段（文件名/目录名），兼容 `/` 与 `\` 分隔符；空串与纯分隔符原样返回。 */
function basename(path: string): string {
  const trimmed = path.trim().replace(/[\\/]+$/, '');
  const i = Math.max(trimmed.lastIndexOf('/'), trimmed.lastIndexOf('\\'));
  return i >= 0 ? trimmed.slice(i + 1) : trimmed;
}

/** 文件类工具别名（含空格版用于匹配 ACP title 前缀，如 "Edit src/a.ts"）。 */
const FILE_ALIASES = [
  'read file', 'read_file', 'read', 'list directory', 'list dir', 'list_dir',
  'edit file', 'edit', 'write file', 'write_file', 'write',
  'patch file', 'patch_file', 'patch',
  'delete', 'remove file', 'remove',
  'move file', 'move', 'rename',
  'code outline', 'code_outline', 'outline',
  'read symbol', 'read_symbol', 'symbol',
];

/** 是否为文件类工具（摘要为文件/目录路径而非命令）。runner 旧格式认
 *  read_file/write_file/patch_file/list_dir 等规范名，ACP 认 kind=read/edit/
 *  delete/move 或 title 前缀（"Edit src/a.ts"）。供 toolSummary 判断摘要类别、
 *  ToolCard 决定是否只显示 basename。 */
function isFileTool(kind: ToolKind | undefined, name: string | undefined): boolean {
  if (kind === 'read' || kind === 'edit' || kind === 'delete' || kind === 'move') return true;
  const nm = (name ?? '').toLowerCase();
  if (['read_file', 'write_file', 'patch_file', 'list_dir', 'read', 'write', 'edit', 'delete', 'move', 'glob', 'code_outline', 'read_symbol', 'outline', 'symbol'].includes(nm)) {
    return true;
  }
  // ACP title 风格：内嵌目标在标题末尾（"Edit src/a.ts"），按文件类别名前缀剥离识别
  return FILE_ALIASES.some((alias) => nm.startsWith(`${alias} `));
}

/** 从 toolArgs JSON 提取一行摘要。
 *
 * 兼容两套数据：runner 旧格式（toolName 是规范工具名 shell/read_file…，字段
 * cmd/path）与 ACP 新格式（toolName 是 title 如 "Bash"/"Edit src/a.ts"，字段
 * command/file_path）。`kind`（ACP tool_kind）优先于 name 判断类别：命令类
 * 认 cmd/command，文件类认 path/file_path，search 认 path+pattern。提取不到
 * 时返回 null（无摘要；标题区已显示 toolName，避免重复）。
 */
function toolSummary(
  name: string | undefined,
  kind: ToolKind | undefined,
  args: string | undefined,
): string | null {
  if (!args || isNoopArgs(args)) return null;
  let parsed: Record<string, unknown>;
  try {
    parsed = JSON.parse(args) as Record<string, unknown>;
  } catch {
    return null;
  }
  const str = (...keys: string[]) => {
    for (const k of keys) {
      const v = parsed[k];
      if (typeof v === 'string' && v) return v;
    }
    return null;
  };
  const nm = (name ?? '').toLowerCase();
  const isExec = kind === 'execute' || ['shell', 'bash', 'execute', 'run', 'sh', 'cmd'].includes(nm);
  if (isExec) return str('cmd', 'command');
  // runner 规范工具名特殊摘要（须在通用 isFileTool 之前，否则被 path 覆盖）
  // read_file 行区间读取：offset/limit 参数时摘要为 basename:offset-end
  // 兼容 number 与可解析为有限正整数的字符串（LLM 有时传 "120"）
  if (nm === 'read_file') {
    const path = str('path', 'file_path');
    const asInt = (v: unknown): number | undefined => {
      if (typeof v === 'number' && Number.isFinite(v) && v >= 0) return Math.floor(v);
      if (typeof v === 'string') {
        const n = Number(v);
        if (Number.isFinite(n) && n >= 0) return Math.floor(n);
      }
      return undefined;
    };
    const offset = asInt(parsed.offset);
    const limit = asInt(parsed.limit);
    if (offset !== undefined || limit !== undefined) {
      const start = offset ?? 1;
      const end = limit !== undefined ? start + limit - 1 : undefined;
      const file = path ? basename(path) : null;
      return file ? (end !== undefined ? `${file}:${start}-${end}` : `${file}:${start}-`) : null;
    }
    return path ?? null;
  }
  // read_symbol：摘要 basename › name
  if (nm === 'read_symbol') {
    const path = str('path', 'file_path');
    const symName = str('name');
    if (path && symName) return `${basename(path)} › ${symName}`;
    return path ?? null;
  }
  // git_* 工具摘要：提取 name/rev/file_path 字段
  if (nm.startsWith('git_')) {
    if (nm === 'git_diff') return str('file_path', 'path');
    if (nm === 'git_show') return str('rev', 'name');
    if (nm === 'git_branch' || nm === 'git_checkout') return str('name');
    return null;
  }
  if (isFileTool(kind, name)) return str('path', 'file_path');
  if (kind === 'search' || ['search', 'grep', 'find'].includes(nm)) {
    const path = str('path', 'file_path') ?? '.';
    const pattern = str('pattern', 'query');
    return pattern ? `${path} ⌕ ${pattern}` : null;
  }
  return null;
}

/** toolKind → 图标 + 语义色 chip 样式。
 *  每种工具类别配独立色相，用户扫一眼图标颜色即可分辨"这次动作是什么"
 *  （读=sky / 写=amber / 删=red / 移=violet / 搜=teal / 执行=emerald /
 *  思考=purple / 抓取=cyan）；switch_mode/缺省归类为"系统动作"，用 muted
 *  灰不抢视觉。chip 采用「淡色圆角底 + 彩色图标」：颜色只是类别标签而非
 *  状态（状态交给徽章 ✓/✗/转圈），底/字双配 light/dark 两套色保证可读。 */
const KIND_STYLE: Record<ToolKind, { icon: typeof Wrench; chipClass: string; iconClass: string }> = {
  read: { icon: FileText, chipClass: 'bg-sky-500/10', iconClass: 'text-sky-600 dark:text-sky-400' },
  edit: { icon: Pencil, chipClass: 'bg-amber-500/10', iconClass: 'text-amber-600 dark:text-amber-400' },
  delete: { icon: Trash2, chipClass: 'bg-red-500/10', iconClass: 'text-red-600 dark:text-red-400' },
  move: { icon: ArrowRightLeft, chipClass: 'bg-violet-500/10', iconClass: 'text-violet-600 dark:text-violet-400' },
  search: { icon: Search, chipClass: 'bg-teal-500/10', iconClass: 'text-teal-600 dark:text-teal-400' },
  execute: { icon: TerminalSquare, chipClass: 'bg-emerald-500/10', iconClass: 'text-emerald-600 dark:text-emerald-400' },
  think: { icon: Brain, chipClass: 'bg-purple-500/10', iconClass: 'text-purple-600 dark:text-purple-400' },
  fetch: { icon: Globe, chipClass: 'bg-cyan-500/10', iconClass: 'text-cyan-600 dark:text-cyan-400' },
  // 系统动作（切换模式/未知）：muted 灰 + bg-muted 底，不参与语义色
  switch_mode: { icon: Wrench, chipClass: 'bg-muted', iconClass: 'text-muted-foreground' },
  other: { icon: Wrench, chipClass: 'bg-muted', iconClass: 'text-muted-foreground' },
};

/** 把 kind 渲染为「淡色圆角底 + 彩色图标」的小 chip：头部图标位统一用 chip，
 *  未知/缺省 kind 落到 other（Wrench 灰）。ThoughtBubble 复用 think 条目保证
 *  思考语义色与工具卡一致。 */
function KindChip({ kind }: { kind: ToolKind }) {
  const style = KIND_STYLE[kind] ?? KIND_STYLE.other;
  const Icon = style.icon;
  return (
    <span className={`flex h-5 w-5 shrink-0 items-center justify-center rounded-md ${style.chipClass}`}>
      <Icon className={`h-3 w-3 ${style.iconClass}`} />
    </span>
  );
}

/** toolKind → 规范显示名 + title 别名表。ACP 各 agent 上报的 title 形态不一
 *  （"Read File"/"Read"/"Edit src/a.ts"/命令本体），统一按 kind 归一为规范名，
 *  title 中内嵌的目标（相对路径/命令）拆出为 extra，避免与 args 摘要重复显示。
 *  `stripPrefix: false`（execute）时仅在 title 恰等于别名时归一——命令本体可能
 *  以 "run"/"bash" 等词开头，前缀剥离会截断真实命令。 */
const KIND_META: Record<ToolKind, { label: string; aliases: string[]; stripPrefix: boolean }> = {
  read: { label: 'Read', aliases: ['read file', 'read_file', 'read', 'list_dir', 'list directory', 'code outline', 'code_outline', 'outline', 'read symbol', 'read_symbol', 'symbol'], stripPrefix: true },
  edit: { label: 'Edit', aliases: ['edit file', 'edit', 'write file', 'write_file', 'write', 'patch file', 'patch_file', 'patch'], stripPrefix: true },
  delete: { label: 'Delete', aliases: ['delete file', 'delete', 'remove file', 'remove'], stripPrefix: true },
  move: { label: 'Move', aliases: ['move file', 'move', 'rename'], stripPrefix: true },
  search: { label: 'Search', aliases: ['search', 'grep', 'glob', 'find'], stripPrefix: true },
  execute: { label: 'Terminal', aliases: ['terminal', 'bash', 'shell', 'sh', 'cmd', 'execute'], stripPrefix: false },
  think: { label: 'Think', aliases: ['think', 'thinking'], stripPrefix: true },
  fetch: { label: 'Fetch', aliases: ['fetch', 'web fetch', 'webfetch', 'browse'], stripPrefix: true },
  switch_mode: { label: 'Mode', aliases: ['switch mode', 'switch_mode', 'mode'], stripPrefix: true },
  other: { label: '', aliases: [], stripPrefix: true },
};

/** runner 规范工具名 → { kind, label } 映射表。kind 缺省/other 时 splitToolTitle
 *  按精确名匹配返回更精确的 label（如 read_file→Read、code_outline→Outline），
 *  KindChip 据此推断语义色（而非 all-gray other）。ACP 路径带显式 toolKind，此表不
 *  干预。 */
const RUNNER_TOOL_META: Record<string, { kind: ToolKind; label: string }> = {
  read_file: { kind: 'read', label: 'Read' },
  list_dir: { kind: 'read', label: 'List' },
  code_outline: { kind: 'read', label: 'Outline' },
  read_symbol: { kind: 'read', label: 'Symbol' },
  write_file: { kind: 'edit', label: 'Write' },
  patch_file: { kind: 'edit', label: 'Patch' },
  shell: { kind: 'execute', label: 'Terminal' },
  search: { kind: 'search', label: 'Search' },
  git_status: { kind: 'other', label: 'Git' },
  git_diff: { kind: 'other', label: 'Git' },
  git_log: { kind: 'other', label: 'Git' },
  git_show: { kind: 'other', label: 'Git' },
  git_branch: { kind: 'other', label: 'Git' },
  git_commit: { kind: 'other', label: 'Git' },
  git_push: { kind: 'other', label: 'Git' },
  git_stage: { kind: 'other', label: 'Git' },
  git_unstage: { kind: 'other', label: 'Git' },
  git_checkout: { kind: 'other', label: 'Git' },
  git_pull: { kind: 'other', label: 'Git' },
  git_revert: { kind: 'other', label: 'Git' },
  git_reset: { kind: 'other', label: 'Git' },
  git_stash: { kind: 'other', label: 'Git' },
  todo_write: { kind: 'think', label: 'Todo' },
  remember: { kind: 'think', label: 'Remember' },
  use_skill: { kind: 'think', label: 'Skill' },
  task: { kind: 'other', label: 'Task' },
};

/** 工具 kind 推断：显式 toolKind 优先；缺省时按 RUNNER_TOOL_META 精确名推断，
 *  仍不命中则回退 'other'。供 KindChip / collectSubagents 等需要语义色的场景。 */
export function effectiveToolKind(name: string | undefined, kind: ToolKind | undefined): ToolKind {
  if (kind && kind !== 'other') return kind;
  const meta = RUNNER_TOOL_META[(name ?? '').toLowerCase()];
  return meta?.kind ?? 'other';
}

/** 归一化工具标题：返回规范显示名 label 与 title 内嵌目标 extra（可能为 null）。
 *  显式 toolKind（非 other）优先匹配；kind 缺省/other（runner 旧数据）时按别名
 *  全表反查推断类别；均不命中且 kind 有效时，title 整体视为 extra（如命令本体）。
 *  导出供 SubagentTaskCard 计算子 agent 正在执行的工具名。 */
export function splitToolTitle(
  name: string | undefined,
  kind: ToolKind | undefined,
): { label: string; extra: string | null } {
  const raw = (name ?? '').trim();
  const lower = raw.toLowerCase();
  // 显式 toolKind（非 other）优先：ACP 路径带 kind 时直接用 KIND_META
  if (kind && kind !== 'other') {
    const meta = KIND_META[kind];
    for (const alias of [...meta.aliases].sort((a, b) => b.length - a.length)) {
      if (lower === alias) return { label: meta.label, extra: null };
      if (meta.stripPrefix && lower.startsWith(`${alias} `)) {
        const extra = raw.slice(alias.length + 1).trim();
        return { label: meta.label, extra: extra || null };
      }
    }
    return { label: meta.label, extra: raw || null };
  }
  // kind 缺省/other：先查 RUNNER_TOOL_META（runner 规范工具名精确匹配，
  // 比 KIND_META 更细粒度，如 list_dir→List 而非 Read）
  if (RUNNER_TOOL_META[lower]) {
    return { label: RUNNER_TOOL_META[lower].label, extra: null };
  }
  // 回退 KIND_META 别名推断（非 RUNNER_TOOL_META 覆盖的旧别名，如 "read file"）
  for (const k of (Object.keys(KIND_META) as ToolKind[]).filter((k) => k !== 'other')) {
    const meta = KIND_META[k];
    for (const alias of [...meta.aliases].sort((a, b) => b.length - a.length)) {
      if (lower === alias) return { label: meta.label, extra: null };
      if (meta.stripPrefix && lower.startsWith(`${alias} `)) {
        const extra = raw.slice(alias.length + 1).trim();
        return { label: meta.label, extra: extra || null };
      }
    }
  }
  return { label: raw || 'Tool', extra: null };
}

/** 工具状态：显式状态优先（failed → failed；completed → completed；显式
 * pending/in_progress/running 原样返回），不再让 toolResult != null 覆盖成
 * completed——Task 父卡的中间态 ToolCallUpdate 常带部分输出（status=running），
 * 旧逻辑把 running+result 直接判 completed，子 agent 没执行完就打勾。
 * 仅 toolStatus 缺省（旧帧/历史 runner 数据）时按 result 有无推断：有→completed，
 * 无→in_progress。导出供 SubagentTaskCard 复用（子 agent 父卡状态徽章）。 */
export function resolveToolStatus(item: ChatItem): 'pending' | 'in_progress' | 'running' | 'completed' | 'failed' {
  const s = item.toolStatus;
  if (s === 'failed') return 'failed';
  if (s === 'completed') return 'completed';
  if (s === 'pending' || s === 'in_progress' || s === 'running') return s;
  return item.toolResult != null ? 'completed' : 'in_progress';
}

/** 工具执行状态徽章：显式 toolStatus 优先；缺省（旧数据）按 result 有无推断。 */
function StatusBadge({ item }: { item: ChatItem }) {
  const status = resolveToolStatus(item);
  if (status === 'failed') return <span className="shrink-0 text-xs text-destructive">✗</span>;
  if (status === 'completed') return <span className="shrink-0 text-xs text-green-600">✓</span>;
  return <Loader2 className="h-3 w-3 shrink-0 animate-spin text-muted-foreground" />;
}

/** 文件路径提示：头部摘要只显示 basename，鼠标悬浮 / 点击（触屏）时用 portal
 *  在视图层弹出完整路径。卡片容器 overflow-hidden 会裁剪绝对定位，故渲染到
 *  body（fixed 定位）；滚动 / 缩放 / 点击外部自动关闭。hover 与点击并存：
 *  鼠标进出 trigger/tip 控制显示，点击 trigger 切换（stopPropagation 防止
 *  误触卡片展开）。下划线点线提示该路径可点开看全貌（触屏无 hover，需要
 *  affordance 暗示可点击）。 */
function PathTip({ path, children }: { path: string; children: ReactNode }) {
  const [open, setOpen] = useState(false);
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null);
  const triggerRef = useRef<HTMLSpanElement>(null);
  const tipRef = useRef<HTMLDivElement>(null);
  const timerRef = useRef(0);
  const id = useId();

  // 基于 trigger 当前布局计算 tip 位置：优先显示在路径下方，下方空间不足时翻转到上方
  const updatePos = useCallback(() => {
    const el = triggerRef.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    const tipW = Math.min(360, window.innerWidth - 16);
    const tipH = 26;
    const left = Math.max(6, Math.min(rect.left, window.innerWidth - tipW - 6));
    const top =
      rect.bottom + tipH + 6 <= window.innerHeight
        ? rect.bottom + 6
        : Math.max(6, rect.top - tipH - 6);
    setPos({ top, left });
  }, []);

  const show = useCallback(() => {
    window.clearTimeout(timerRef.current);
    timerRef.current = 0;
    updatePos();
    setOpen(true);
  }, [updatePos]);

  const hide = useCallback(() => {
    window.clearTimeout(timerRef.current);
    timerRef.current = window.setTimeout(() => setOpen(false), 100);
  }, []);

  const toggle = useCallback(() => {
    window.clearTimeout(timerRef.current);
    timerRef.current = 0;
    updatePos();
    setOpen((v) => !v);
  }, [updatePos]);

  // 打开期间：点击外部关闭；滚动/窗口变化关闭（fixed 定位需重算，直接隐藏更稳）
  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: PointerEvent) => {
      if (triggerRef.current?.contains(e.target as Node)) return;
      if (tipRef.current?.contains(e.target as Node)) return;
      setOpen(false);
    };
    const onDismiss = () => setOpen(false);
    document.addEventListener('pointerdown', onPointerDown);
    document.addEventListener('scroll', onDismiss, true);
    window.addEventListener('resize', onDismiss);
    return () => {
      document.removeEventListener('pointerdown', onPointerDown);
      document.removeEventListener('scroll', onDismiss, true);
      window.removeEventListener('resize', onDismiss);
    };
  }, [open]);

  return (
    <>
      <span
        ref={triggerRef}
        id={id}
        aria-describedby={open ? `${id}-tip` : undefined}
        className="min-w-0 cursor-pointer truncate font-mono text-muted-foreground underline decoration-dotted decoration-muted-foreground/40 underline-offset-2"
        onMouseEnter={show}
        onMouseLeave={hide}
        onClick={(e) => {
          e.stopPropagation();
          toggle();
        }}
      >
        {children}
      </span>
      {open && pos && (
        createPortal(
          <div
            ref={tipRef}
            id={`${id}-tip`}
            role="tooltip"
            className="pointer-events-auto fixed z-50 max-w-[360px] truncate rounded-md border bg-popover px-2 py-1 font-mono text-xs text-popover-foreground shadow-md"
            style={{ top: pos.top, left: pos.left }}
            onMouseEnter={show}
            onMouseLeave={hide}
          >
            {path}
          </div>,
          document.body,
        )
      )}
    </>
  );
}

/** 工具调用卡片：默认收起为一行头部（图标 + 名称 + 摘要 + 状态），点击展开详情。
 *  详情优先级：diffs → args/result 文本（折叠）。导出供 SubagentTaskCard 嵌套渲染
 *  子 agent 的工具卡（迷你卡）。 */
export function ToolCard({ item }: { item: ChatItem }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const { label, extra } = splitToolTitle(item.toolName, item.toolKind);
  // 摘要只显示一份：args 提取的结构化摘要优先（通常是绝对路径/命令），
  // 缺失时才用 title 内嵌目标（通常是相对路径）——两者同源，同显即用户反馈的
  // 「标题双重路径」问题；两者都无路径（如 ACP Edit 的 raw_input 为空占位、
  // 路径只经 content Diff/locations 到达）时，退回 diffs/locations 的首个路径，
  // 保证文件操作卡片头部始终能看到目标文件。
  const summary =
    toolSummary(item.toolName, item.toolKind, item.toolArgs) ??
    extra ??
    item.toolDiffs?.[0]?.path ??
    item.toolLocations?.[0]?.path ??
    null;
  // 文件类工具的摘要是路径：头部只显示文件名，完整路径挂 PathTip（悬浮/点击查看）
  const isFile = isFileTool(item.toolKind, item.toolName);
  const displaySummary = isFile && summary ? basename(summary) : summary;
  const status = resolveToolStatus(item);
  const isError = status === 'failed';
  // 不确定进度条：工具仍在执行（pending/in_progress/running，result 未产出）时
  // 在卡片头部下方显示一条呼吸动画进度条，替代静态转圈的区域性弱提示
  const isRunning = status !== 'completed' && status !== 'failed';

  return (
    <div>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-2 text-left text-xs"
        aria-expanded={open}
      >
        <KindChip kind={effectiveToolKind(item.toolName, item.toolKind)} />
        <span className="font-medium text-foreground/90">{label}</span>
        {displaySummary && (
          isFile ? (
            <PathTip path={summary as string}>{displaySummary}</PathTip>
          ) : (
            <span className="min-w-0 truncate font-mono text-muted-foreground">{displaySummary}</span>
          )
        )}
        <span className="ml-auto flex shrink-0 items-center gap-1.5">
          <StatusBadge item={item} />
          {open ? (
            <ChevronUp className="h-3.5 w-3.5 text-muted-foreground" />
          ) : (
            <ChevronDown className="h-3.5 w-3.5 text-muted-foreground" />
          )}
        </span>
      </button>
      {/* 进度条绝对定位在卡片底部边缘：运行中显示呼吸动画，完成后透明淡出。
          不占用文档流高度（卡片高度由头部内容决定，内容垂直居中）；容器常驻
          避免进度条出现/消失时 DOM 抖动。 */}
      <div
        className={`absolute inset-x-0 bottom-0 h-0.5 overflow-hidden rounded transition-colors duration-300 ${
          isRunning ? 'bg-muted' : 'bg-transparent'
        }`}
        aria-hidden={!isRunning}
      >
        {isRunning && <div className="h-full w-1/3 animate-pulse rounded bg-primary/60" />}
      </div>
      {open && (
        <div className="mt-2 space-y-2 border-t border-border/60 pt-2">
          {item.toolDiffs && item.toolDiffs.length > 0 && <ToolDiffView diffs={item.toolDiffs} />}
          {item.toolArgs && !isNoopArgs(item.toolArgs) && <CollapsiblePre text={item.toolArgs} />}
          {item.toolResult ? (
            <CollapsiblePre text={item.toolResult} className={item.toolArgs || item.toolDiffs ? 'border-t border-border/60 pt-2' : undefined} />
          ) : isRunning ? (
            // 仅运行中才显示「执行中」：结果为空但状态已是终态（completed/failed）时
            // 不再误显执行中（M5）——历史 JSON tool_result 的失败工具 status=failed
            // 且结果可能为空串，卡片只保留 ✗ 徽章 + 失败提示。
            <div className="mt-1 flex items-center gap-1 text-xs text-muted-foreground">
              <Loader2 className="h-3 w-3 animate-spin" />
              {t('agent.toolRunning')}
            </div>
          ) : null}
          {isError && item.toolResult && (
            <div className="text-xs text-destructive">{t('agent.toolFailed')}</div>
          )}
        </div>
      )}
    </div>
  );
}

/** 单条消息气泡：user / assistant（Markdown）/ tool（默认收起的工具卡片）。
 *  memo 化：流式 chunk 每帧更新列表 state，内容未变的气泡跳过重渲染。
 *  `streaming`：当前正在流式写入的气泡。流式期间 assistant/thought 走
 *  `<Markdown streaming />`——仍是 Streamdown 渲染（加粗/标题/列表/表格结构
 *  保留），只是去掉 code 插件避免每帧 Shiki 全量重高亮（O(n²)，见 Markdown.tsx
 *  注释），终态后切回完整 Markdown 一次性高亮。
 *  布局策略（对标主流 AI 聊天 UI）：
 *  - user：右对齐小气泡（primary 淡底），用户消息一般短，气泡让双方身份一眼可辨
 *  - assistant：全宽无气泡正文。LLM 回复是长文（标题/列表/表格/代码块），
 *    套 max-w-[80%] 的圆角盒子会压缩排版空间、且圆角背景让长文显得拥挤
 *  - tool：全宽细线卡片，视觉上弱于正文（工具是过程，正文是结论） */
export default memo(function MessageBubble({ item, streaming }: { item: ChatItem; streaming?: boolean }) {
  const cls =
    item.kind === 'user'
      ? 'ml-auto max-w-[85%] rounded-2xl rounded-br-md bg-primary/10 px-3.5 py-2 text-sm leading-relaxed'
      : item.kind === 'assistant'
        ? 'w-full py-0.5'
        : item.kind === 'thought'
          ? 'w-full rounded-lg border border-border/70 bg-muted/30 px-3 py-2 text-sm'
          : item.kind === 'plan'
            ? 'w-full rounded-lg border border-border/70 bg-muted/30 px-3 py-2 text-sm'
            : item.kind === 'attachment'
              ? 'w-full rounded-lg border border-border/70 bg-muted/30 px-3 py-2 text-sm'
            // tool 卡片：relative 供底部边缘进度条定位；overflow-hidden 让进度条
            // 在圆角内被裁剪，不超出卡片边框
            : 'relative w-full overflow-hidden rounded-lg border border-border/70 bg-muted/30 px-3 py-2 text-sm';

  return (
    <div className={cls}>
      {item.kind === 'tool' ? (
        <ToolCard item={item} />
      ) : item.kind === 'assistant' ? (
        <Markdown content={item.content} streaming={streaming} />
      ) : item.kind === 'thought' ? (
        <ThoughtBubble content={item.content} streaming={streaming} />
      ) : item.kind === 'plan' ? (
        <PlanBubble entries={item.planEntries ?? []} />
      ) : item.kind === 'attachment' ? (
        <AttachmentBubble item={item} />
      ) : (
        <div className="whitespace-pre-wrap break-words">{item.content}</div>
      )}
    </div>
  );
});

/** ACP 多模态占位卡（image/audio/resource）：只带元信息（不透传 base64 数据），
 *  表达「agent 在此输出了一份附件」。有 uri 时渲染为链接（新标签页打开）。 */
function AttachmentBubble({ item }: { item: ChatItem }) {
  const { t } = useTranslation();
  const kind = item.attachmentKind ?? 'resource';
  const Icon =
    kind === 'image' ? ImageIcon : kind === 'audio' ? FileAudio : Paperclip;
  const label =
    kind === 'image'
      ? t('agent.attachmentImage')
      : kind === 'audio'
        ? t('agent.attachmentAudio')
        : t('agent.attachmentResource');
  const name = item.attachmentName || label;
  const body = (
    <>
      <Icon className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
      <span className="shrink-0 text-xs font-medium text-foreground/90">{label}</span>
      <span className="min-w-0 truncate text-xs text-muted-foreground">{name}</span>
      {item.attachmentMime && (
        <span className="ml-auto shrink-0 text-[10px] text-muted-foreground/70">
          {item.attachmentMime}
        </span>
      )}
    </>
  );
  if (item.attachmentUri) {
    return (
      <a
        href={item.attachmentUri}
        target="_blank"
        rel="noreferrer"
        className="flex items-center gap-2 hover:underline"
      >
        {body}
      </a>
    );
  }
  return <div className="flex items-center gap-2">{body}</div>;
}

/** 思考内容通常是 Markdown：取首个非空行（剥掉标题/列表/引用前缀与行内强调
 *  符号）作折叠态预览——预览是纯文本，残留 `**` 等标记会显得 noisy。 */
function thoughtPreview(content: string): string | null {
  const line = content.split('\n').find((l) => l.trim());
  if (!line) return null;
  const text = line
    .trim()
    .replace(/^(#{1,6}\s+|[-*+]\s+|>\s*)/, '')
    .replace(/[*_`~]/g, '')
    .trim();
  return text || null;
}

/** 思考过程卡片：与 ToolCard 同构（图标 + 标题 + 预览 + chevron 头部，
 *  展开区 border-t 分隔），默认折叠（思考是低信噪过程信息）。
 *  内容按 Markdown 渲染（agent 思考多为 md 格式），muted 弱化以区分正文。
 *  `streaming` 期间展开区走 `<Markdown streaming />`（同正文气泡：保留 md 结构、
 *  去掉 code 插件避免流式每帧 Shiki 重高亮）。 */
function ThoughtBubble({ content, streaming }: { content: string; streaming?: boolean }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const preview = thoughtPreview(content);
  return (
    <div>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-2 text-left text-xs"
        aria-expanded={open}
      >
        {/* 思考气泡的 Brain 与工具 kind=think 语义一致，复用其紫色 chip */}
        <KindChip kind="think" />
        <span className="font-medium text-foreground/90">{t('agent.thought')}</span>
        {!open && preview && (
          <span className="min-w-0 truncate text-muted-foreground">{preview}</span>
        )}
        <span className="ml-auto flex shrink-0 items-center gap-1.5">
          {open ? (
            <ChevronUp className="h-3.5 w-3.5 text-muted-foreground" />
          ) : (
            <ChevronDown className="h-3.5 w-3.5 text-muted-foreground" />
          )}
        </span>
      </button>
      {open && (
        <div className="mt-2 border-t border-border/60 pt-2 text-muted-foreground [&_pre]:!my-2">
          <Markdown content={content} streaming={streaming} />
        </div>
      )}
    </div>
  );
}

const PLAN_MARK: Record<string, { mark: string; cls: string }> = {
  completed: { mark: '✓', cls: 'text-green-600' },
  in_progress: { mark: '▶', cls: 'text-primary' },
  pending: { mark: '○', cls: 'text-muted-foreground' },
};

/** plan 条目优先级 → 语义色徽标（参照 KindChip 配色风格）。 */
const PRIORITY_BADGE: Record<string, { label: string; cls: string }> = {
  high: { label: 'H', cls: 'bg-red-500/15 text-red-600 dark:text-red-400' },
  medium: { label: 'M', cls: 'bg-yellow-500/15 text-yellow-600 dark:text-yellow-400' },
  low: { label: 'L', cls: 'bg-muted text-muted-foreground' },
};

/** 计划气泡：checklist 样式；新 plan 帧由 ChatStream 就地更新本气泡内容。 */
function PlanBubble({ entries }: { entries: PlanEntryItem[] }) {
  const { t } = useTranslation();
  return (
    <div>
      <div className="mb-1 flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
        <ListChecks className="h-3.5 w-3.5" />
        {t('agent.plan')}
      </div>
      <ul className="space-y-0.5">
        {entries.map((e, i) => {
          const mark = PLAN_MARK[e.status] ?? PLAN_MARK.pending;
          const prio = e.priority ? PRIORITY_BADGE[e.priority] : null;
          return (
            <li key={i} className="flex items-baseline gap-2 text-xs">
              <span className={mark.cls}>{mark.mark}</span>
              <span className={e.status === 'completed' ? 'text-muted-foreground line-through' : ''}>
                {e.content}
              </span>
              {prio && (
                <span className={`inline-flex h-4 shrink-0 items-center rounded px-1 text-[10px] font-medium leading-none ${prio.cls}`}>
                  {prio.label}
                </span>
              )}
            </li>
          );
        })}
      </ul>
    </div>
  );
}

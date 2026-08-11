import { memo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
  Brain,
  ChevronDown,
  ChevronUp,
  FileText,
  Globe,
  ListChecks,
  Loader2,
  Pencil,
  Search,
  TerminalSquare,
  Wrench,
} from 'lucide-react';
import type { ChatItem, ToolKind, PlanEntryItem } from './types';
import Markdown from './Markdown';
import ToolDiffView from './ToolDiffView';

/** 折叠阈值：tool 参数/结果超过该行数时只显示前 3 行，可手动展开。 */
const COLLAPSE_LINE_THRESHOLD = 6;
const COLLAPSE_VISIBLE_LINES = 3;

function firstLines(text: string, n: number): string {
  const lines = text.split('\n');
  if (lines.length <= n) return text;
  return lines.slice(0, n).join('\n');
}

/** 工具调用的长文本（args/result）：超过阈值折叠为前 3 行 + 展开按钮。 */
function CollapsiblePre({ text, className }: { text: string; className?: string }) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  const lineCount = text.split('\n').length;
  const collapsible = lineCount > COLLAPSE_LINE_THRESHOLD;
  const shown = collapsible && !expanded ? firstLines(text, COLLAPSE_VISIBLE_LINES) : text;

  return (
    <div className={className}>
      <pre className="whitespace-pre-wrap text-xs text-muted-foreground">{shown}</pre>
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
  const isFile =
    kind === 'read' || kind === 'edit' || kind === 'delete' || kind === 'move' ||
    ['read_file', 'write_file', 'patch_file', 'list_dir', 'read', 'write', 'edit', 'delete', 'move', 'glob'].includes(nm);
  if (isFile) return str('path', 'file_path');
  if (kind === 'search' || ['search', 'grep', 'find'].includes(nm)) {
    const path = str('path', 'file_path') ?? '.';
    const pattern = str('pattern', 'query');
    return pattern ? `${path} ⌕ ${pattern}` : null;
  }
  return null;
}

/** toolKind → 图标（视觉分类；未知/缺省一律 Wrench）。 */
const KIND_ICON: Record<ToolKind, typeof Wrench> = {
  read: FileText,
  edit: Pencil,
  delete: Pencil,
  move: Pencil,
  search: Search,
  execute: TerminalSquare,
  think: Brain,
  fetch: Globe,
  switch_mode: Wrench,
  other: Wrench,
};

/** toolKind → 规范显示名 + title 别名表。ACP 各 agent 上报的 title 形态不一
 *  （"Read File"/"Read"/"Edit src/a.ts"/命令本体），统一按 kind 归一为规范名，
 *  title 中内嵌的目标（相对路径/命令）拆出为 extra，避免与 args 摘要重复显示。
 *  `stripPrefix: false`（execute）时仅在 title 恰等于别名时归一——命令本体可能
 *  以 "run"/"bash" 等词开头，前缀剥离会截断真实命令。 */
const KIND_META: Record<ToolKind, { label: string; aliases: string[]; stripPrefix: boolean }> = {
  read: { label: 'Read', aliases: ['read file', 'read_file', 'read', 'list_dir', 'list directory'], stripPrefix: true },
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

/** 归一化工具标题：返回规范显示名 label 与 title 内嵌目标 extra（可能为 null）。
 *  显式 toolKind（非 other）优先匹配；kind 缺省/other（runner 旧数据）时按别名
 *  全表反查推断类别；均不命中且 kind 有效时，title 整体视为 extra（如命令本体）。 */
function splitToolTitle(
  name: string | undefined,
  kind: ToolKind | undefined,
): { label: string; extra: string | null } {
  const raw = (name ?? '').trim();
  const lower = raw.toLowerCase();
  const candidates: ToolKind[] =
    kind && kind !== 'other'
      ? [kind]
      : (Object.keys(KIND_META) as ToolKind[]).filter((k) => k !== 'other');
  for (const k of candidates) {
    const meta = KIND_META[k];
    // 长别名优先（"read file" 先于 "read"）
    for (const alias of [...meta.aliases].sort((a, b) => b.length - a.length)) {
      if (lower === alias) return { label: meta.label, extra: null };
      if (meta.stripPrefix && lower.startsWith(`${alias} `)) {
        const extra = raw.slice(alias.length + 1).trim();
        return { label: meta.label, extra: extra || null };
      }
    }
  }
  if (kind && kind !== 'other') {
    return { label: KIND_META[kind].label, extra: raw || null };
  }
  return { label: raw || 'Tool', extra: null };
}

/** 工具状态：failed 优先；result 已产出 → completed（ACP 的 ToolCallUpdate 常省略
 * status，上游若误映射为 running，result 到达后仍应显示完成）；缺省按 result 有无推断。 */
function resolveToolStatus(item: ChatItem): 'pending' | 'in_progress' | 'running' | 'completed' | 'failed' {
  if (item.toolStatus === 'failed') return 'failed';
  if (item.toolResult != null) return 'completed';
  return item.toolStatus ?? 'in_progress';
}

/** 工具执行状态徽章：显式 toolStatus 优先；缺省（旧数据）按 result 有无推断。 */
function StatusBadge({ item }: { item: ChatItem }) {
  const status = resolveToolStatus(item);
  if (status === 'failed') return <span className="shrink-0 text-xs text-destructive">✗</span>;
  if (status === 'completed') return <span className="shrink-0 text-xs text-green-600">✓</span>;
  return <Loader2 className="h-3 w-3 shrink-0 animate-spin text-muted-foreground" />;
}

/** 工具调用卡片：默认收起为一行头部（图标 + 名称 + 摘要 + 状态），点击展开详情。
 *  详情优先级：diffs → args/result 文本（折叠）。 */
function ToolCard({ item }: { item: ChatItem }) {
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
  const Icon = KIND_ICON[item.toolKind ?? 'other'] ?? Wrench;
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
        <Icon className="h-3.5 w-3.5 shrink-0 text-primary" />
        <span className="font-medium text-foreground/90">{label}</span>
        {summary && (
          <span className="min-w-0 truncate font-mono text-muted-foreground">{summary}</span>
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
          ) : (
            <div className="mt-1 flex items-center gap-1 text-xs text-muted-foreground">
              <Loader2 className="h-3 w-3 animate-spin" />
              {t('agent.toolRunning')}
            </div>
          )}
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
 *  布局策略（对标主流 AI 聊天 UI）：
 *  - user：右对齐小气泡（primary 淡底），用户消息一般短，气泡让双方身份一眼可辨
 *  - assistant：全宽无气泡正文。LLM 回复是长文（标题/列表/表格/代码块），
 *    套 max-w-[80%] 的圆角盒子会压缩排版空间、且圆角背景让长文显得拥挤
 *  - tool：全宽细线卡片，视觉上弱于正文（工具是过程，正文是结论） */
export default memo(function MessageBubble({ item }: { item: ChatItem }) {
  const cls =
    item.kind === 'user'
      ? 'ml-auto max-w-[85%] rounded-2xl rounded-br-md bg-primary/10 px-3.5 py-2 text-sm leading-relaxed'
      : item.kind === 'assistant'
        ? 'w-full py-0.5'
        : item.kind === 'thought'
          ? 'w-full rounded-lg border border-border/70 bg-muted/30 px-3 py-2 text-sm'
          : item.kind === 'plan'
            ? 'w-full rounded-lg border border-border/70 bg-muted/30 px-3 py-2 text-sm'
            // tool 卡片：relative 供底部边缘进度条定位；overflow-hidden 让进度条
            // 在圆角内被裁剪，不超出卡片边框
            : 'relative w-full overflow-hidden rounded-lg border border-border/70 bg-muted/30 px-3 py-2 text-sm';

  return (
    <div className={cls}>
      {item.kind === 'tool' ? (
        <ToolCard item={item} />
      ) : item.kind === 'assistant' ? (
        <Markdown content={item.content} />
      ) : item.kind === 'thought' ? (
        <ThoughtBubble content={item.content} />
      ) : item.kind === 'plan' ? (
        <PlanBubble entries={item.planEntries ?? []} />
      ) : (
        <div className="whitespace-pre-wrap">{item.content}</div>
      )}
    </div>
  );
});

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
 *  内容按 Markdown 渲染（agent 思考多为 md 格式），muted 弱化以区分正文。 */
function ThoughtBubble({ content }: { content: string }) {
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
        <Brain className="h-3.5 w-3.5 shrink-0 text-primary" />
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
          <Markdown content={content} />
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
          return (
            <li key={i} className="flex items-baseline gap-2 text-xs">
              <span className={mark.cls}>{mark.mark}</span>
              <span className={e.status === 'completed' ? 'text-muted-foreground line-through' : ''}>
                {e.content}
              </span>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

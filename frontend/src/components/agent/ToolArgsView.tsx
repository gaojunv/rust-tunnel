import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import type { ToolKind } from './types';
import { CollapsiblePre, effectiveToolKind } from './MessageBubble';

/** 短标量阈值：单行且 ≤120 字符的 string 直接一行 kv；更长的走折叠块。 */
const SHORT_STRING_MAX = 120;

/** runner read_file 结果标记行：`[showing lines X-Y of N]`（可能出现在末尾，
 *  截断提示行之后，或个别实现的首行）。剥出做 caption + 行号起始。 */
const READ_MARKER_RE = /\[showing lines\s+(\d+)\s*-\s*(\d+)\s+of\s+(\d+)\]/;

/** ACP claude-code Read 输出自带行号前缀（形如 `   123→code` 或 `123│code`）：
 *  命中首个非空行则原样透传，不重复加行号。 */
const ACP_LINE_PREFIX_RE = /^\s*\d+\s*[→│|]/;

function parseArgsObject(args: string): Record<string, unknown> | null {
  try {
    const v: unknown = JSON.parse(args);
    if (v && typeof v === 'object' && !Array.isArray(v)) return v as Record<string, unknown>;
    return null;
  } catch {
    return null;
  }
}

/** 从 args JSON 解析 read 起始行号 offset（兼容 number 与可解析的字符串），
 *  非法/缺省返回 null（调用方按 1 处理或不用 gutter）。 */
function parseArgsOffset(args?: string): number | null {
  if (!args) return null;
  try {
    const v: unknown = JSON.parse(args);
    if (v && typeof v === 'object' && !Array.isArray(v)) {
      const o = (v as Record<string, unknown>).offset;
      const n = typeof o === 'number' ? o : typeof o === 'string' ? Number(o) : NaN;
      if (Number.isFinite(n) && n >= 1) return Math.floor(n);
    }
  } catch {
    /* 非 JSON → 无 offset */
  }
  return null;
}

/** 剥出 runner 标记行：返回标记（含 from/to/total）与去掉标记后的正文。 */
function extractReadMarker(lines: string[]): {
  marker: { from: number; to: number; total: number } | null;
  body: string;
} {
  const idx = lines.findIndex((l) => READ_MARKER_RE.test(l.trim()));
  if (idx === -1) return { marker: null, body: lines.join('\n') };
  const m = READ_MARKER_RE.exec(lines[idx].trim());
  if (!m) return { marker: null, body: lines.join('\n') };
  const marker = { from: Number(m[1]), to: Number(m[2]), total: Number(m[3]) };
  const body = [...lines.slice(0, idx), ...lines.slice(idx + 1)].join('\n');
  return { marker, body };
}

/** 一行 kv：key（muted）+ `:` + value（mono，break-all 防溢出）。 */
function Kv({ name, value }: { name: string; value: string }) {
  return (
    <div className="text-xs">
      <span className="text-muted-foreground">{name}</span>
      <span className="text-muted-foreground">:</span>{' '}
      <span className="break-all font-mono">{value}</span>
    </div>
  );
}

/** 单个字段：短标量一行 kv；长/多行 string、对象/数组 → 字段名小标签 + 折叠块。 */
function Field({ name, value }: { name: string; value: unknown }) {
  if (typeof value === 'string') {
    if (!value) return null;
    if (value.includes('\n') || value.length > SHORT_STRING_MAX) {
      return (
        <div>
          <div className="text-xs text-muted-foreground">{name}</div>
          <CollapsiblePre text={value} />
        </div>
      );
    }
    return <Kv name={name} value={value} />;
  }
  if (typeof value === 'number' || typeof value === 'boolean') {
    return <Kv name={name} value={String(value)} />;
  }
  // 对象 / 数组
  return (
    <div>
      <div className="text-xs text-muted-foreground">{name}</div>
      <CollapsiblePre text={JSON.stringify(value, null, 2)} />
    </div>
  );
}

/** 通用兜底：顶层字段逐行渲染，跳过 null/undefined/空串。 */
function Fields({ entries }: { entries: [string, unknown][] }) {
  const rows = entries.filter(
    ([, v]) => !(v === null || v === undefined || (typeof v === 'string' && v === '')),
  );
  if (rows.length === 0) return null;
  return (
    <div className="space-y-1">
      {rows.map(([key, value]) => (
        <Field key={key} name={key} value={value} />
      ))}
    </div>
  );
}

/**
 * 结构化工具参数视图：把 toolArgs 的 raw JSON 转成人可读的分层展示。
 * - execute 类：description 说明行 + 命令块（多行 heredoc 原样换行），其余标量按通用规则
 * - 其他：顶层字段短标量一行 kv、长文本/嵌套结构折叠块
 * - JSON 解析失败或非 plain object → 回退原文折叠（CollapsiblePre）
 */
export function ToolArgsView({ name, kind, args }: { name?: string; kind?: ToolKind; args: string }) {
  const parsed = parseArgsObject(args);
  if (!parsed) return <CollapsiblePre text={args} />;
  if (effectiveToolKind(name, kind) === 'execute') {
    const cmd =
      typeof parsed.cmd === 'string' && parsed.cmd !== ''
        ? parsed.cmd
        : typeof parsed.command === 'string' && parsed.command !== ''
          ? parsed.command
          : null;
    if (cmd) {
      const description = typeof parsed.description === 'string' ? parsed.description : '';
      const rest = Object.entries(parsed).filter(
        ([k]) => k !== 'cmd' && k !== 'command' && k !== 'description',
      );
      return (
        <div className="space-y-1.5">
          {description && <div className="text-xs text-muted-foreground">{description}</div>}
          <pre className="whitespace-pre-wrap break-words rounded-md border border-border/60 bg-muted/60 px-2.5 py-1.5 font-mono text-xs">
            {cmd}
          </pre>
          {rest.length > 0 && <Fields entries={rest} />}
        </div>
      );
    }
  }
  return <Fields entries={Object.entries(parsed)} />;
}

/** 纯代码块（search 结果、ACP 自带行号的 read）：圆角边框 + max-h-72 滚动，无 gutter。 */
function CodeBlock({ text }: { text: string }) {
  return (
    <div className="overflow-hidden rounded-md border border-border/60">
      <div className="max-h-72 overflow-auto whitespace-pre px-2 py-1 font-mono text-xs leading-5">
        {text}
      </div>
    </div>
  );
}

/** read 结果：marker 剥出做 caption + 行号 gutter；ACP 自带行号则原样透传。 */
function ReadResult({ result, args }: { result: string; args?: string }) {
  const { t } = useTranslation();
  const lines = result.split('\n');
  const { marker, body } = extractReadMarker(lines);
  const bodyLines = body.split('\n');
  const acpPrefixed = ACP_LINE_PREFIX_RE.test(bodyLines.find((l) => l.trim()) ?? '');
  // 起始行号：marker 的 X 优先；否则 args 的 offset（缺省 1 但仍需要「显式」
  // 依据才启用 gutter——全量读取无 marker 无 offset 时退化为无行号代码块）
  const start = marker ? marker.from : (parseArgsOffset(args) ?? null);
  const numbered = start != null && !acpPrefixed;
  const caption = marker
    ? t('agent.readLinesRange', { from: marker.from, to: marker.to, total: marker.total })
    : null;
  if (acpPrefixed) return <CodeBlock text={body} />;
  return (
    <div className="overflow-hidden rounded-md border border-border/60">
      {caption && (
        <div className="border-b border-border/60 bg-muted/50 px-2 py-0.5 text-xs text-muted-foreground">
          {caption}
        </div>
      )}
      <div className="max-h-72 overflow-auto py-1">
        {bodyLines.map((line, i) => (
          <div key={i} className="flex items-baseline">
            {numbered && (
              <span className="w-10 shrink-0 select-none pr-2 text-right font-mono text-xs leading-5 text-muted-foreground/60">
                {(start as number) + i}
              </span>
            )}
            <span className="whitespace-pre font-mono text-xs leading-5">{line}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

/**
 * 结构化工具结果视图：按工具类别选择展示形态。
 * - execute：终端深色输出块（折叠复用 CollapsiblePre 阈值逻辑，preClassName 换肤）
 * - read：marker caption + 行号 gutter（ACP 自带行号原样透传）
 * - search：代码块样式（path:line: 匹配 文本）
 * - 其他：原文折叠（CollapsiblePre）
 * `className` 透传给外层 div（ToolCard 用 border-t 分隔）。
 */
export function ToolResultView({
  name,
  kind,
  args,
  result,
  className,
}: {
  name?: string;
  kind?: ToolKind;
  args?: string;
  result: string;
  className?: string;
}) {
  let content: ReactNode;
  const eff = effectiveToolKind(name, kind);
  if (eff === 'execute') {
    content = (
      <CollapsiblePre
        text={result}
        preClassName="rounded-md bg-zinc-950 px-2.5 py-1.5 font-mono text-zinc-100"
      />
    );
  } else if (eff === 'read') {
    content = <ReadResult result={result} args={args} />;
  } else if (eff === 'search') {
    content = <CodeBlock text={result} />;
  } else {
    content = <CollapsiblePre text={result} />;
  }
  return <div className={className}>{content}</div>;
}

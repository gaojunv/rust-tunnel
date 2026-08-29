import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Folder } from 'lucide-react';
import type { ToolKind } from './types';
import { CollapsiblePre, effectiveToolKind, splitCdCommand } from './MessageBubble';

/** 短标量阈值：单行且 ≤120 字符的 string 直接一行 kv；更长的走折叠块。 */
const SHORT_STRING_MAX = 120;

/** runner read_file 结果标记行：`[showing lines X-Y of N]`（可能出现在末尾，
 *  截断提示行之后，或个别实现的首行）。剥出做 caption + 行号起始。 */
const READ_MARKER_RE = /\[showing lines\s+(\d+)\s*-\s*(\d+)\s+of\s+(\d+)\]/;

/** ACP agent 的 Read 输出自带行号前缀。实测格式：
 *  - claude-code：`spaces + 行号 + Tab`（终端把 Tab 渲染成 `→`，原文是 \t；
 *    只认 →/│/| 会漏判 → 我们再叠一层 gutter 就是用户反馈的「双行号」）
 *  - 其他 agent 可能是 `123│code` / `123|code` / `123: code`
 *  冒号形态有误判面（日志行 `12:30:45` 也命中），故单行命中不足为凭——
 *  hasOwnLineNumbers 用前 3 个非空行多数确认。 */
const ACP_LINE_PREFIX_RE = /^\s*\d+\s*(?:[→│|:]|\t)/;

/** 结果正文是否已自带行号：取前 3 个非空行，多数（单行样本则唯一行）命中
 *  行号前缀即判定透传。多行确认防日志/冒号文本的偶发误判。 */
function hasOwnLineNumbers(bodyLines: string[]): boolean {
  const sample = bodyLines.filter((l) => l.trim()).slice(0, 3);
  if (sample.length === 0) return false;
  const hits = sample.filter((l) => ACP_LINE_PREFIX_RE.test(l)).length;
  return sample.length === 1 ? hits === 1 : hits >= 2;
}

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

/** execute 类参数的执行目录：命令内 `cd <dir> && ` 前缀目录优先（agent 常 prepend，
 *  见 splitCdCommand），否则 opencode 的 workdir / runner shell 的 cwd 字段。 */
function execWorkDir(parsed: Record<string, unknown>, cdDir: string | null): string | null {
  if (cdDir) return cdDir;
  for (const k of ['workdir', 'cwd'] as const) {
    const v = parsed[k];
    if (typeof v === 'string' && v.trim()) return v.trim();
  }
  return null;
}

/**
 * 结构化工具参数视图：把 toolArgs 的 raw JSON 转成人可读的分层展示。
 * - execute 类：description 说明行 + 执行目录行（basename 显示完整路径）+ 命令块
 *   （多行 heredoc 原样换行，命令内 `cd <dir> && ` 前缀剥出归入目录行），其余标量按通用规则
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
      const { command, cdDir } = splitCdCommand(cmd);
      const workDir = execWorkDir(parsed, cdDir);
      const description = typeof parsed.description === 'string' ? parsed.description : '';
      const rest = Object.entries(parsed).filter(
        ([k]) => k !== 'cmd' && k !== 'command' && k !== 'description' && k !== 'workdir' && k !== 'cwd',
      );
      return (
        <div className="space-y-1.5">
          {description && <div className="text-xs text-muted-foreground">{description}</div>}
          {workDir && (
            <div className="flex min-w-0 items-center gap-1 text-xs text-muted-foreground">
              <Folder className="h-3 w-3 shrink-0" />
              <span className="truncate font-mono">{workDir}</span>
            </div>
          )}
          <pre className="whitespace-pre-wrap break-words rounded-md border border-border/60 bg-muted/60 px-2.5 py-1.5 font-mono text-xs">
            {command}
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

/** read 结果：marker 剥出做 caption + 行号 gutter；已自带行号（ACP 各 agent
 *  的 Read 输出）则原样透传——caption 仍保留（marker 信息独立于行号）。 */
function ReadResult({ result, args }: { result: string; args?: string }) {
  const { t } = useTranslation();
  const lines = result.split('\n');
  const { marker, body } = extractReadMarker(lines);
  const bodyLines = body.split('\n');
  const acpPrefixed = hasOwnLineNumbers(bodyLines);
  // 起始行号：marker 的 X 优先；否则 args 的 offset（缺省 1 但仍需要「显式」
  // 依据才启用 gutter——全量读取无 marker 无 offset 时退化为无行号代码块）
  const start = marker ? marker.from : (parseArgsOffset(args) ?? null);
  const numbered = start != null && !acpPrefixed;
  const caption = marker
    ? t('agent.readLinesRange', { from: marker.from, to: marker.to, total: marker.total })
    : null;
  if (acpPrefixed) {
    return (
      <div className="overflow-hidden rounded-md border border-border/60">
        {caption && (
          <div className="border-b border-border/60 bg-muted/50 px-2 py-0.5 text-xs text-muted-foreground">
            {caption}
          </div>
        )}
        <div className="max-h-72 overflow-auto whitespace-pre px-2 py-1 font-mono text-xs leading-5">
          {body}
        </div>
      </div>
    );
  }
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

/** 尝试把 execute 结果解析为 opencode bash 的结构化输出 `{stdout, stderr, exitCode}`
 *  （失败时 ShellError.info 同构，stdout/stderr 经 toJSON 已是字符串）。仅当对象含
 *  至少一个已知键才判定——普通纯文本 JSON.parse 失败即返回 null，不会误伤。 */
function parseExecResultObject(
  result: string,
): { stdout: string; stderr: string; exitCode: number | null } | null {
  let v: unknown;
  try {
    v = JSON.parse(result);
  } catch {
    return null;
  }
  if (!v || typeof v !== 'object' || Array.isArray(v)) return null;
  const o = v as Record<string, unknown>;
  if (!('stdout' in o) && !('stderr' in o) && !('exitCode' in o)) return null;
  const str = (x: unknown): string => (typeof x === 'string' ? x : typeof x === 'number' ? String(x) : '');
  return {
    stdout: str(o.stdout),
    stderr: str(o.stderr),
    exitCode: typeof o.exitCode === 'number' ? o.exitCode : null,
  };
}

/** opencode bash 执行结果（`{stdout, stderr, exitCode}` JSON）的结构化展示：
 *  exitCode 徽章 + stdout 主输出 + stderr 报错（红色区分）。非该形态回退终端
 *  深色文本。 */
function ExecResult({ result }: { result: string }) {
  const { t } = useTranslation();
  const parsed = parseExecResultObject(result);
  if (!parsed) {
    return (
      <CollapsiblePre
        text={result}
        preClassName="rounded-md bg-zinc-950 px-2.5 py-1.5 font-mono text-zinc-100"
      />
    );
  }
  const { stdout, stderr, exitCode } = parsed;
  return (
    <div className="space-y-1.5">
      {exitCode != null && (
        <div className={exitCode === 0 ? 'text-xs text-green-600' : 'text-xs text-destructive'}>
          {exitCode === 0 ? '✓' : '✗'} {t('agent.exitCode', { code: exitCode })}
        </div>
      )}
      {stdout && (
        <CollapsiblePre
          text={stdout}
          preClassName="rounded-md bg-zinc-950 px-2.5 py-1.5 font-mono text-zinc-100"
        />
      )}
      {stderr && (
        <CollapsiblePre
          text={stderr}
          preClassName="rounded-md bg-zinc-950 px-2.5 py-1.5 font-mono text-red-400"
        />
      )}
      {!stdout && !stderr && (
        <div className="text-xs text-muted-foreground">{t('agent.noOutput')}</div>
      )}
    </div>
  );
}

/**
 * 结构化工具结果视图：按工具类别选择展示形态。
 * - execute：opencode bash 的 `{stdout,stderr,exitCode}` JSON 结构化；其余走终端深色输出块
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
    content = <ExecResult result={result} />;
  } else if (eff === 'read') {
    content = <ReadResult result={result} args={args} />;
  } else if (eff === 'search') {
    content = <CodeBlock text={result} />;
  } else {
    content = <CollapsiblePre text={result} />;
  }
  return <div className={className}>{content}</div>;
}

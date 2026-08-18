import { useMemo } from 'react';
import ReactCodeMirror, { basicSetup, type Extension } from '@uiw/react-codemirror';
import { oneDark } from '@codemirror/theme-one-dark';
import { linter, type Diagnostic } from '@codemirror/lint';
import { rust } from '@codemirror/lang-rust';
import { javascript } from '@codemirror/lang-javascript';
import { python } from '@codemirror/lang-python';
import { go } from '@codemirror/lang-go';
import { markdown } from '@codemirror/lang-markdown';
import { json } from '@codemirror/lang-json';
import { yaml } from '@codemirror/lang-yaml';
import { css } from '@codemirror/lang-css';
import { html } from '@codemirror/lang-html';
import { sql } from '@codemirror/lang-sql';
import { xml } from '@codemirror/lang-xml';
import { load as loadYaml } from 'js-yaml';

/**
 * CodeMirror 6 文件编辑器（受控 React 组件）。
 * - 语法高亮按路径扩展名选择语言；json/yaml 挂实时 lint（行级错误下划线）。
 * - jsdom（测试）/ 无 ResizeObserver 环境退化为纯文本 `<pre>`，避免构造
 *   CodeMirror 时依赖未实现的环境 API。
 */

/** 环境是否足够支撑 CodeMirror：jsdom 或无 ResizeObserver 视为不支持。 */
export function isEditorSupported(): boolean {
  if (typeof ResizeObserver === 'undefined') return false;
  if (/jsdom/i.test(navigator.userAgent)) return false;
  return true;
}

/** 按文件扩展名映射 CodeMirror 语言扩展；无匹配/需 legacy-modes 的语言退化为纯文本。 */
export function editorLanguageForPath(path: string): Extension {
  const ext = path.split('.').pop()?.toLowerCase() ?? '';
  switch (ext) {
    case 'rs':
      return rust();
    case 'ts':
    case 'tsx':
      return javascript({ typescript: true, jsx: true });
    case 'js':
    case 'jsx':
      return javascript({ jsx: true });
    case 'py':
      return python();
    case 'go':
      return go();
    case 'md':
      return markdown();
    case 'json':
      return json();
    case 'yaml':
    case 'yml':
      return yaml();
    case 'css':
      return css();
    case 'html':
      return html();
    case 'sql':
      return sql();
    case 'xml':
      return xml();
    default:
      // sh/toml/java/c/cpp/h 等需 @codemirror/legacy-modes，未安装时保持纯文本
      return [];
  }
}

/** 定位 JSON.parse 报错位置：优先解析 error.message 中的行/偏移，兜底文档开头。 */
function jsonErrorPosition(message: string, docText: string): number {
  const pos = /position (\d+)/i.exec(message);
  if (pos) return Math.min(Number(pos[1]), docText.length);
  // Firefox/Safari 风格：`at line N column M`
  const line = /line (\d+)/i.exec(message);
  if (line) {
    const target = Math.max(1, Number(line[1]));
    const lines = docText.split('\n');
    let offset = 0;
    for (let i = 0; i < Math.min(target - 1, lines.length); i++) {
      offset += lines[i].length + 1;
    }
    return Math.min(offset, docText.length);
  }
  return 0;
}

/** json 实时 lint：空内容不报错；解析失败给出定位到行首的一条诊断。 */
const jsonLintExt = linter((view): Diagnostic[] => {
  const text = view.state.doc.toString();
  if (!text.trim()) return [];
  try {
    JSON.parse(text);
    return [];
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    const pos = jsonErrorPosition(message, text);
    const lineStart = view.state.doc.lineAt(pos).from;
    return [{ from: lineStart, to: lineStart, severity: 'error', message }];
  }
});

/** 定位 YAMLException 的报错行：从 "at line N, column M" 提取 N 并换算为文档偏移。 */
function yamlErrorPosition(message: string, docText: string): number {
  const line = /line (\d+)/i.exec(message);
  if (line) {
    const target = Math.max(1, Number(line[1]));
    const lines = docText.split('\n');
    let offset = 0;
    for (let i = 0; i < Math.min(target - 1, lines.length); i++) {
      offset += lines[i].length + 1;
    }
    return Math.min(offset, docText.length);
  }
  return 0;
}

/** yaml 实时 lint：js-yaml 解析失败给出一条诊断（提示带原始 message）。 */
const yamlLintExt = linter((view): Diagnostic[] => {
  const text = view.state.doc.toString();
  if (!text.trim()) return [];
  try {
    loadYaml(text);
    return [];
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    const pos = yamlErrorPosition(message, text);
    const from = view.state.doc.lineAt(pos).from;
    const snippet = message.length > 200 ? `${message.slice(0, 200)}…` : message;
    return [{ from, to: from, severity: 'error', message: `YAML: ${snippet}` }];
  }
});

interface CodeMirrorEditorProps {
  value: string;
  onChange?: (value: string) => void;
  path: string;
  isDark: boolean;
  readOnly?: boolean;
}

export default function CodeMirrorEditor({
  value,
  onChange,
  path,
  isDark,
  readOnly = false,
}: CodeMirrorEditorProps) {
  // 扩展组合：basicSetup（行号/折叠/括号匹配/高亮当前行）+ 语言 + lint（json/yaml）。
  // 注意依赖提取的包是 @uiw/react-codemirror 内置的同款 basicSetup，故显式 basicSetup={false}。
  const extensions = useMemo(() => {
    const ext = path.split('.').pop()?.toLowerCase() ?? '';
    const linters: Extension[] = [];
    if (ext === 'json') linters.push(jsonLintExt);
    if (ext === 'yaml' || ext === 'yml') linters.push(yamlLintExt);
    return [basicSetup(), editorLanguageForPath(path), ...linters];
  }, [path]);

  // 测试/jsdom 环境退化：不构造 CodeMirror，直接渲染纯文本（保持内容可断言）。
  if (!isEditorSupported()) {
    return <pre className="whitespace-pre-wrap p-2 font-mono text-xs leading-5">{value}</pre>;
  }

  return (
    <div className="h-full min-h-0 overflow-hidden" data-testid="codemirror-editor">
      <ReactCodeMirror
        value={value}
        onChange={onChange}
        extensions={extensions}
        basicSetup={false}
        theme={isDark ? oneDark : 'light'}
        readOnly={readOnly}
        height="100%"
        style={{ height: '100%' }}
      />
    </div>
  );
}
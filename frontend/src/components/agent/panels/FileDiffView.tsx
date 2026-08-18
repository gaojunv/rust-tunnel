import { useEffect, useMemo, useRef } from 'react';
import { MergeView } from '@codemirror/merge';
import { EditorState, type Extension } from '@codemirror/state';
import { EditorView } from '@codemirror/view';
import { oneDark } from '@codemirror/theme-one-dark';
import { editorLanguageForPath, isEditorSupported } from '../CodeMirrorEditor';

/**
 * 并排 diff 视图：左 = 打开文件时拉取的已保存内容（只读），右 = 当前草稿（可编辑）。
 * 用 @codemirror/merge 的 MergeView 底层 API 手动挂载两个 EditorView，两端同一语言、
 * 同一主题；右侧编辑实时冒泡 onDraftChange，外部 draft/saved 变化时同步对应侧 doc。
 * jsdom（测试）环境退化为展示两区块文本的 `<pre>`。
 */

/** MergeView 外层（.cm-mergeView）不随编辑器自支撑高度，由容器决定，需显式撑满。 */
const mergeHeightTheme = EditorView.baseTheme({
  '.cm-mergeView': { height: '100%' },
});

interface FileDiffViewProps {
  /** 打开时拉取的已保存内容（只读侧） */
  saved: string;
  /** 当前草稿（可编辑侧） */
  draft: string;
  /** 右侧编辑内容变化回调 */
  onDraftChange: (value: string) => void;
  path: string;
  isDark: boolean;
}

export default function FileDiffView({ saved, draft, onDraftChange, path, isDark }: FileDiffViewProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const mergeRef = useRef<MergeView | null>(null);
  // 最新 draft 快照：updateListener 里对比，跳过「程序化同步导致的重复回调」（避免回环）
  const draftRef = useRef(draft);
  draftRef.current = draft;

  const language = useMemo(() => editorLanguageForPath(path), [path]);
  const themeExt = useMemo<Extension>(() => (isDark ? oneDark : []), [isDark]);

  // 挂载 MergeView（两端同为编辑语言 + 主题；saved/draft 变化由下方 effect 增量同步）
  useEffect(() => {
    const container = containerRef.current;
    if (!isEditorSupported() || !container) return;
    let mv: MergeView | null = null;
    try {
      mv = new MergeView({
        a: {
          doc: saved,
          extensions: [
            language,
            themeExt,
            mergeHeightTheme,
            EditorState.readOnly.of(true),
            EditorView.editable.of(false),
          ],
        },
        b: {
          doc: draft,
          extensions: [
            language,
            themeExt,
            mergeHeightTheme,
            EditorView.updateListener.of((update) => {
              if (update.docChanged) {
                const text = update.state.doc.toString();
                if (text !== draftRef.current) onDraftChange(text);
              }
            }),
          ],
        },
        orientation: 'a-b',
        parent: container,
      });
      mergeRef.current = mv;
    } catch {
      // 构造失败（异常环境）静默降级为空白区域，不影响文件面板其余功能
      mergeRef.current = null;
    }
    return () => {
      mv?.destroy();
      mergeRef.current = null;
      container.textContent = '';
    };
    // saved/draft 走下方同步 effect，避免重建视图导致滚动/选择丢失
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [language, themeExt, onDraftChange]);

  // 外部 saved 变化（保存成功后刷新）→ 同步左侧只读 doc
  useEffect(() => {
    const mv = mergeRef.current;
    if (!mv) return;
    const cur = mv.a.state.doc.toString();
    if (cur !== saved) {
      mv.a.dispatch({ changes: { from: 0, to: cur.length, insert: saved } });
    }
  }, [saved]);

  // 外部 draft 变化（保存/取消等非输入路径）→ 同步右侧编辑 doc
  useEffect(() => {
    const mv = mergeRef.current;
    if (!mv) return;
    const cur = mv.b.state.doc.toString();
    if (cur !== draft) {
      mv.b.dispatch({ changes: { from: 0, to: cur.length, insert: draft } });
    }
  }, [draft]);

  if (!isEditorSupported()) {
    return (
      <pre
        data-testid="diff-fallback"
        className="min-h-0 flex-1 overflow-auto whitespace-pre-wrap p-2 font-mono text-xs leading-5"
      >
        <div>saved:</div>
        <div>{saved}</div>
        <div>draft:</div>
        <div>{draft}</div>
      </pre>
    );
  }

  return <div ref={containerRef} data-testid="file-diff" className="min-h-0 flex-1 overflow-hidden" />;
}
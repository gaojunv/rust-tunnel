/**
 * [[ wikilink 自动补全 —— 挂在 NoteEditor 的 textarea（编辑态）
 * 取舍说明：
 * - 代码块判定仅检查该行是否以 ``` 或 4 空格开头（简易近似，注释说明取舍）：
 *   真实 Markdown 围栏可带语言标记或缩进，非围栏缩进 4 空格也视为代码，足够覆盖常见情况，
 *   复杂嵌套/列表内代码等边界不处理，避免在前端重复实现完整 Markdown 解析。
 * - 下拉定位用 measureCaretInTextarea 的 mirror-div 法，边界 clamp 防溢出。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listNotes } from "@/api/tauri";
import type { NoteSummary } from "@/api/types";
import { fuzzyScore } from "@/lib/fuzzy";
import { measureCaretInTextarea } from "@/lib/caret-position";
import { buildInsertion, findLinkQuery } from "@/lib/wikilink-complete";

type Props = {
  textareaRef: React.RefObject<HTMLTextAreaElement>;
  containerRef: React.RefObject<HTMLElement>;
  refreshToken: number;
  isEdit: boolean;
};

export function WikilinkAutocomplete({ textareaRef, containerRef, refreshToken, isEdit }: Props) {
  const [notes, setNotes] = useState<NoteSummary[] | null>(null);
  const [queryInfo, setQueryInfo] = useState<{ start: number; query: string } | null>(null);
  const [activeIndex, setActiveIndex] = useState(0);
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null);
  const queryInfoRef = useRef(queryInfo);
  queryInfoRef.current = queryInfo;

  // 缓存候选：挂载 + refreshToken 变化时 listNotes()
  useEffect(() => {
    let cancelled = false;
    listNotes()
      .then((data) => {
        if (!cancelled) setNotes(data);
      })
      .catch(() => {
        if (!cancelled) setNotes([]);
      });
    return () => {
      cancelled = true;
    };
  }, [refreshToken]);

  const candidates = useMemo(() => {
    if (!queryInfo || !notes) return [];
    const q = queryInfo.query;
    if (q === "") {
      return [...notes].sort((a, b) => b.modified - a.modified).slice(0, 8);
    }
    type Scored = { note: NoteSummary; score: number };
    const scored: Scored[] = [];
    for (const n of notes) {
      const sKey = fuzzyScore(n.key, q);
      const sTitle = fuzzyScore(n.title, q);
      let best: number | null = null;
      if (sKey !== null) best = sKey;
      if (sTitle !== null && (best === null || sTitle > best)) best = sTitle;
      if (best !== null) scored.push({ note: n, score: best });
    }
    scored.sort((a, b) => b.score - a.score || b.note.modified - a.note.modified);
    return scored.slice(0, 8).map((s) => s.note);
  }, [queryInfo, notes]);

  useEffect(() => {
    setActiveIndex(0);
  }, [candidates, queryInfo]);

  const updateQuery = useCallback(() => {
    const ta = textareaRef.current;
    if (!ta || !isEdit) {
      setQueryInfo(null);
      setPos(null);
      return;
    }
    const caret = ta.selectionStart ?? 0;
    const info = findLinkQuery(ta.value, caret);
    setQueryInfo(info);
    if (info) {
      try {
        const caretPos = measureCaretInTextarea(ta, caret);
        const container = containerRef.current;
        let top: number;
        let left: number;
        if (container) {
          const cRect = container.getBoundingClientRect();
          const taRect = ta.getBoundingClientRect();
          // 下拉位于 caret 下一行
          const lh = 20;
          top = taRect.top - cRect.top + caretPos.top + lh + 4;
          left = taRect.left - cRect.left + caretPos.left;
          // 边界 clamp（下拉宽约 280）
          const dropdownW = 280;
          const maxLeft = Math.max(4, container.clientWidth - dropdownW - 4);
          left = Math.max(4, Math.min(left, maxLeft));
          const maxTop = container.clientHeight - 200;
          if (top > maxTop) top = Math.max(4, taRect.top - cRect.top + caretPos.top - 180);
          // scrollOffset：容器可能滚动，measure 已含 scrollTop，容器绝对定位无需额外补偿
        } else {
          top = caretPos.top + 20;
          left = caretPos.left;
        }
        setPos({ top, left });
      } catch {
        setPos(null);
      }
    } else {
      setPos(null);
    }
  }, [textareaRef, containerRef, isEdit]);

  // 监听 textarea 事件：onChange/onKeyUp/onClick 后更新
  useEffect(() => {
    if (!isEdit) {
      setQueryInfo(null);
      return;
    }
    const ta = textareaRef.current;
    if (!ta) return;
    const handle = () => {
      // 下一帧再测，确保 selectionStart 已更新
      requestAnimationFrame(updateQuery);
    };
    ta.addEventListener("input", handle);
    ta.addEventListener("keyup", handle);
    ta.addEventListener("click", handle);
    ta.addEventListener("select", handle);
    // 初始检查
    updateQuery();
    return () => {
      ta.removeEventListener("input", handle);
      ta.removeEventListener("keyup", handle);
      ta.removeEventListener("click", handle);
      ta.removeEventListener("select", handle);
    };
  }, [isEdit, textareaRef, updateQuery]);

  const close = useCallback(() => {
    setQueryInfo(null);
    setPos(null);
  }, []);

  const handleSelect = useCallback(
    (key: string) => {
      const ta = textareaRef.current;
      const info = queryInfoRef.current;
      if (!ta || !info) return;
      const caret = ta.selectionStart ?? ta.value.length;
      const insertion = buildInsertion(key, info.query);
      // 替换 [[query 段（start..caret）
      const start = info.start;
      const end = caret;
      ta.setRangeText(insertion, start, end, "end");
      // 同步到 React 受控状态：派发 input 事件让 onChange 捕获
      const ev = new Event("input", { bubbles: true });
      ta.dispatchEvent(ev);
      // 直接触发一次 updateQuery 关闭下拉
      const newPos = start + insertion.length;
      requestAnimationFrame(() => {
        try {
          ta.setSelectionRange(newPos, newPos);
        } catch {
          // 忽略
        }
        ta.focus();
        close();
      });
    },
    [textareaRef, close],
  );

  // 键盘拦截：补全打开时 Enter/Tab/Esc 等
  useEffect(() => {
    const ta = textareaRef.current;
    if (!ta) return;
    const handler = (e: KeyboardEvent) => {
      const info = queryInfoRef.current;
      if (!info || candidates.length === 0) return;
      if (e.key === "ArrowDown") {
        e.preventDefault();
        e.stopPropagation();
        setActiveIndex((i) => (i + 1) % candidates.length);
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        e.stopPropagation();
        setActiveIndex((i) => (i - 1 + candidates.length) % candidates.length);
      } else if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        e.stopPropagation();
        const cur = candidates[activeIndex];
        if (cur) handleSelect(cur.key);
      } else if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        close();
      }
    };
    ta.addEventListener("keydown", handler);
    return () => ta.removeEventListener("keydown", handler);
  }, [textareaRef, candidates, activeIndex, handleSelect, close]);

  // 失焦关闭（延迟，避免点击下拉时先失焦）
  useEffect(() => {
    const ta = textareaRef.current;
    if (!ta) return;
    const onBlur = () => {
      setTimeout(() => {
        const active = document.activeElement;
        if (active && containerRef.current?.contains(active as Node)) return;
        // 若焦点在下拉内不关闭
        const dropdown = document.getElementById("wikilink-autocomplete");
        if (dropdown && dropdown.contains(active as Node)) return;
        // 否则保持开启由 updateQuery 决定，这里不强制关闭，避免光标移动误关
      }, 150);
    };
    ta.addEventListener("blur", onBlur);
    return () => ta.removeEventListener("blur", onBlur);
  }, [textareaRef, containerRef]);

  if (!isEdit || !queryInfo || candidates.length === 0) return null;

  return (
    <div
      id="wikilink-autocomplete"
      className="absolute z-30 max-h-48 w-[280px] overflow-auto rounded-md border bg-popover shadow-lg"
      style={pos ? { top: pos.top, left: pos.left } : { top: 40, left: 8 }}
      role="listbox"
    >
      <ul className="p-1">
        {candidates.map((n, idx) => {
          const isActive = idx === activeIndex;
          return (
            <li key={n.key}>
              <button
                type="button"
                role="option"
                aria-selected={isActive}
                onMouseEnter={() => setActiveIndex(idx)}
                onMouseDown={(e) => {
                  e.preventDefault();
                  handleSelect(n.key);
                }}
                onClick={() => handleSelect(n.key)}
                className={`flex w-full flex-col gap-0.5 rounded px-2.5 py-1.5 text-left text-xs ${isActive ? "bg-accent" : "hover:bg-accent/60"}`}
              >
                <span className="line-clamp-1 font-medium">{n.title}</span>
                <span className="truncate text-xs text-muted-foreground">{n.key}</span>
              </button>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

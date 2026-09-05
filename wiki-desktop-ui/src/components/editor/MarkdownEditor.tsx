import {
  forwardRef,
  useEffect,
  useImperativeHandle,
  useRef,
  useMemo,
  type RefObject,
} from "react";
import { EditorState } from "@codemirror/state";
import {
  EditorView,
  keymap,
  drawSelection,
  highlightActiveLine,
  placeholder as cmPlaceholder,
} from "@codemirror/view";
import { history, historyKeymap, defaultKeymap } from "@codemirror/commands";
import { searchKeymap } from "@codemirror/search";
import { completionKeymap, autocompletion, startCompletion } from "@codemirror/autocomplete";
import { markdown, markdownLanguage } from "@codemirror/lang-markdown";
import { languages } from "@codemirror/language-data";
import { search } from "@codemirror/search";
import { Prec } from "@codemirror/state";
import { cn } from "@/lib/utils";
import { wikiTheme, wikiSyntaxHighlighting } from "@/lib/codemirror/theme";
import {
  toggleBold,
  toggleItalic,
  toggleStrikethrough,
  toggleInlineCode,
} from "@/lib/codemirror/format-commands";
import {
  toggleBulletListCommand,
  toggleOrderedListCommand,
  toggleTaskListCommand,
} from "@/lib/codemirror/format-commands";
import {
  wikilinkCompletionSource,
  type NoteSummaryLike,
} from "@/lib/codemirror/wikilink-source";
import { insertImageMarkdown } from "@/lib/codemirror/format-commands";

export type MarkdownEditorProps = {
  initialDoc: string;
  onDocChanged?: (doc: string) => void;
  onSave?: () => void;
  getCompletionNotes?: () => NoteSummaryLike[];
  onPasteImage?: (file: File) => Promise<string | null>;
  onImageError?: (message: string) => void;
  placeholder?: string;
  className?: string;
};

export interface MarkdownEditorHandle {
  view(): EditorView | null;
}

function useLatest<T>(value: T): RefObject<T> {
  const ref = useRef(value);
  ref.current = value;
  return ref;
}

function isImageFile(file: File): boolean {
  return file.type.startsWith("image/");
}

function collectImageFiles(list: FileList | null | undefined): File[] {
  if (!list) return [];
  const out: File[] = [];
  for (let i = 0; i < list.length; i++) {
    const f = list[i];
    if (isImageFile(f)) out.push(f);
  }
  return out;
}

export const MarkdownEditor = forwardRef<MarkdownEditorHandle, MarkdownEditorProps>(
  function MarkdownEditor(
    {
      initialDoc,
      onDocChanged,
      onSave,
      getCompletionNotes,
      onPasteImage,
      onImageError,
      placeholder,
      className,
    },
    ref,
  ) {
    const containerRef = useRef<HTMLDivElement>(null);
    const viewRef = useRef<EditorView | null>(null);

    const onDocChangedRef = useLatest(onDocChanged);
    const onSaveRef = useLatest(onSave);
    const onPasteImageRef = useLatest(onPasteImage);
    const onImageErrorRef = useLatest(onImageError);
    const getCompletionNotesRef = useLatest(getCompletionNotes);

    const initialDocRef = useRef(initialDoc);
    const placeholderRef = useRef(placeholder);

    // keep latest completions getter stable but allow refresh via closure ref
    const completionSource = useMemo(
      () => wikilinkCompletionSource(() => (getCompletionNotesRef.current?.() ?? [])),
      // Intentionally not depending on getCompletionNotes identity; closure reads latest via ref
      // eslint-disable-next-line react-hooks/exhaustive-deps
      [],
    );

    useImperativeHandle(ref, () => ({
      view: () => viewRef.current,
    }));

    useEffect(() => {
      const parent = containerRef.current;
      if (!parent) return;

      const updateListener = EditorView.updateListener.of((update) => {
        if (update.docChanged) {
          onDocChangedRef.current?.(update.state.doc.toString());
        }
      });

      const pasteDropHandlers = EditorView.domEventHandlers({
        paste(event: ClipboardEvent, view: EditorView): boolean {
          const handler = onPasteImageRef.current;
          if (!handler) return false;
          const files = collectImageFiles(event.clipboardData?.files);
          if (files.length === 0) return false;
          event.preventDefault();
          (async () => {
            for (const file of files) {
              try {
                const path = await handler(file);
                if (path) {
                  const snippet = insertImageMarkdown(path);
                  view.dispatch(view.state.replaceSelection(snippet));
                }
              } catch (e) {
                const msg = e instanceof Error ? e.message : String(e);
                onImageErrorRef.current?.(msg || "粘贴图片失败");
              }
            }
          })();
          return true;
        },
        drop(event: DragEvent, view: EditorView): boolean {
          const handler = onPasteImageRef.current;
          if (!handler) return false;
          const files = collectImageFiles(event.dataTransfer?.files);
          if (files.length === 0) return false;
          event.preventDefault();
          let dropPos: number | null = null;
          try {
            dropPos = view.posAtCoords({ x: event.clientX, y: event.clientY });
          } catch {
            dropPos = null;
          }
          (async () => {
            for (const file of files) {
              try {
                const path = await handler(file);
                if (path) {
                  const snippet = insertImageMarkdown(path);
                  if (dropPos !== null) {
                    const tr = view.state.update({
                      changes: { from: dropPos, insert: snippet },
                      selection: { anchor: dropPos + snippet.length },
                    });
                    view.dispatch(tr);
                    dropPos += snippet.length;
                  } else {
                    view.dispatch(view.state.replaceSelection(snippet));
                  }
                }
              } catch (e) {
                const msg = e instanceof Error ? e.message : String(e);
                onImageErrorRef.current?.(msg || "拖拽图片失败");
              }
            }
          })();
          return true;
        },
      });

      // Mod modifier: Cmd on mac, Ctrl elsewhere — CM handles this natively via "Mod-"
      const saveKeymap = Prec.high(
        keymap.of([
          {
            key: "Mod-s",
            run: () => {
              onSaveRef.current?.();
              return true;
            },
          },
          { key: "Mod-b", run: toggleBold },
          { key: "Mod-i", run: toggleItalic },
          { key: "Mod-Shift-x", run: toggleStrikethrough },
          { key: "Mod-Shift-7", run: toggleOrderedListCommand },
          { key: "Mod-Shift-8", run: toggleBulletListCommand },
          { key: "Mod-Shift-9", run: toggleTaskListCommand },
        ]),
      );

      const defaultMaps = keymap.of([
        ...defaultKeymap,
        ...historyKeymap,
        ...searchKeymap,
        ...completionKeymap,
      ]);

      const extensions = [
        history(),
        drawSelection(),
        EditorState.allowMultipleSelections.of(true),
        highlightActiveLine(),
        markdown({ base: markdownLanguage, codeLanguages: languages }),
        wikiSyntaxHighlighting,
        autocompletion({
          override: [completionSource],
          icons: false,
          activateOnTyping: true,
        }),
        // also enable explicit trigger for wikilinks: start completion on "[["
        EditorView.updateListener.of((update) => {
          if (!update.docChanged) return;
          const doc = update.state.doc.toString();
          const pos = update.state.selection.main.head;
          // if last two chars before cursor are "[[", trigger completion
          if (pos >= 2 && doc.slice(pos - 2, pos) === "[[") {
            startCompletion(update.view);
          }
        }),
        search({ top: true }),
        saveKeymap,
        defaultMaps,
        EditorView.lineWrapping,
        wikiTheme,
        placeholderRef.current ? cmPlaceholder(placeholderRef.current) : [],
        updateListener,
        pasteDropHandlers,
        // prevent Codemirror from handling Mod-s as save-to-disk
        Prec.highest(
          keymap.of([
            {
              key: "Mod-s",
              run: () => {
                onSaveRef.current?.();
                return true;
              },
            },
          ]),
        ),
      ];

      // suppress unused variable warning for toggleInlineCode if not bound — keep import for toolbar reuse
      void toggleInlineCode;

      const state = EditorState.create({
        doc: initialDocRef.current,
        extensions,
      });

      const view = new EditorView({ state, parent });
      viewRef.current = view;

      return () => {
        view.destroy();
        viewRef.current = null;
      };
      // mount once; initialDoc changes handled by parent remounting via key={noteKey}
      // eslint-disable-next-line react-hooks/exhaustive-deps
    }, []);

    return <div ref={containerRef} className={cn("min-h-0 flex-1 overflow-hidden", className)} />;
  },
);

import { useCallback, useEffect, useRef, useState } from 'react';
import { useRoles } from '../../api/hooks';
import type { AgentRole } from '../../types';
import { useImeGuard } from '@/hooks/useImeGuard';
import MentionPopup from './MentionPopup';
import SlashCommandPopup from './SlashCommandPopup';
import type { SlashCommand } from './SlashCommandPopup';

export interface ChatInputProps {
  input: string;
  onInputChange: (v: string) => void;
  refs: string[];
  setRefs: React.Dispatch<React.SetStateAction<string[]>>;
  workspaceId: string;
  slashCommands: SlashCommand[];
  textareaRef: React.RefObject<HTMLTextAreaElement>;
  onSend: () => void;
  placeholder?: string;
}

export default function ChatInput({
  input,
  onInputChange,
  refs,
  setRefs,
  workspaceId,
  slashCommands,
  textareaRef,
  onSend,
  placeholder,
}: ChatInputProps) {
  const [mention, setMention] = useState<{ start: number; query: string } | null>(null);
  const [mentionFiles, setMentionFiles] = useState<string[]>([]);
  const [mentionActiveIdx, setMentionActiveIdx] = useState(0);
  const [slashMention, setSlashMention] = useState<{ start: number; query: string } | null>(null);
  const [slashActiveIdx, setSlashActiveIdx] = useState(0);
  const [slashFilteredCommands, setSlashFilteredCommands] = useState<SlashCommand[]>([]);
  const blurTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const composingRef = useRef(false);
  const lineHeightCacheRef = useRef<number | null>(null);
  const ime = useImeGuard();
  const { data: rolesData } = useRoles({ enabled: true });
  const roles: AgentRole[] = rolesData?.roles ?? [];

  // Invalidate cached lineHeight when font-related layout may have changed (responsive breakpoint / resize)
  useEffect(() => {
    const onResize = () => {
      lineHeightCacheRef.current = null;
    };
    globalThis.addEventListener('resize', onResize);
    return () => globalThis.removeEventListener('resize', onResize);
  }, []);

  const autoresizeInput = useCallback(() => {
    const el = textareaRef.current;
    if (!el) return;
    if (composingRef.current) return;
    el.style.height = 'auto';
    if (el.scrollHeight === 0) return;
    let lh = lineHeightCacheRef.current;
    if (lh == null) {
      lh = parseFloat(getComputedStyle(el).lineHeight) || 20;
      lineHeightCacheRef.current = lh;
    }
    const max = lh * 10 + 16;
    el.style.height = `${Math.min(el.scrollHeight, max)}px`;
    el.style.overflowY = el.scrollHeight > max ? 'auto' : 'hidden';
  }, [textareaRef]);

  useEffect(() => {
    autoresizeInput();
  }, [input, autoresizeInput]);

  const closeMention = useCallback(() => {
    setMention(null);
    setMentionFiles([]);
    setMentionActiveIdx(0);
  }, []);

  const closeSlashMention = useCallback(() => {
    setSlashMention(null);
    setSlashFilteredCommands([]);
    setSlashActiveIdx(0);
  }, []);

  const selectMention = useCallback(
    (path: string) => {
      if (!mention) return;
      const before = input.slice(0, mention.start);
      const after = input.slice(mention.start + 1 + mention.query.length);
      if (path.startsWith('@')) {
        onInputChange(before + path + ' ' + after);
      } else {
        onInputChange(before + after);
        setRefs((prev) => (prev.includes(path) ? prev : [...prev, path]));
      }
      closeMention();
      if (blurTimerRef.current) {
        clearTimeout(blurTimerRef.current);
        blurTimerRef.current = null;
      }
      textareaRef.current?.focus();
    },
    [mention, input, onInputChange, setRefs, closeMention, textareaRef],
  );

  const selectSlashCommand = useCallback(
    (name: string) => {
      if (!slashMention) return;
      const before = input.slice(0, slashMention.start);
      const after = input.slice(slashMention.start + 1 + slashMention.query.length);
      onInputChange(before + '/' + name + ' ' + after);
      closeSlashMention();
      if (blurTimerRef.current) {
        clearTimeout(blurTimerRef.current);
        blurTimerRef.current = null;
      }
      textareaRef.current?.focus();
    },
    [slashMention, input, onInputChange, closeSlashMention, textareaRef],
  );

  const handleMentionFilesChange = useCallback((files: string[]) => {
    setMentionFiles(files);
  }, []);
  const handleMentionActiveIdxChange = useCallback((idx: number) => {
    setMentionActiveIdx(idx);
  }, []);
  const handleSlashCommandsChange = useCallback((cmds: SlashCommand[]) => {
    setSlashFilteredCommands(cmds);
  }, []);
  const handleSlashActiveIdxChange = useCallback((idx: number) => {
    setSlashActiveIdx(idx);
  }, []);

  const handleInputChange = (e: React.ChangeEvent<HTMLTextAreaElement>) => {
    const v = e.target.value;
    onInputChange(v);
    const pos = e.target.selectionStart ?? v.length;
    const before = v.slice(0, pos);
    const at = before.lastIndexOf('@');
    if (at >= 0 && (at === 0 || /\s/.test(before[at - 1]))) {
      const q = before.slice(at + 1);
      if (!/\s/.test(q)) {
        closeSlashMention();
        setMention({ start: at, query: q });
        return;
      }
    }
    closeMention();
    if (slashCommands.length > 0 && (before === '/' || (before.startsWith('/') && !before.slice(1).includes(' ')))) {
      setSlashMention({ start: 0, query: before.slice(1) });
      return;
    }
    closeSlashMention();
  };

  const handleCompositionStart = useCallback(() => {
    composingRef.current = true;
    ime.bind.onCompositionStart();
  }, [ime.bind]);

  const handleCompositionEnd = useCallback(() => {
    ime.bind.onCompositionEnd();
    // Delay clearing to cover Safari's compositionend -> keydown sequence
    globalThis.setTimeout(() => {
      composingRef.current = false;
      autoresizeInput();
    }, 0);
  }, [ime.bind, autoresizeInput]);

  return (
    <>
      {refs.length > 0 && (
        <div className="flex flex-wrap gap-1 px-2 pt-1.5">
          {refs.map((r) => (
            <span key={r} className="inline-flex items-center gap-1 rounded-md bg-primary/10 px-2 py-0.5 text-xs text-primary">
              @{r}
              <button type="button" onClick={() => setRefs((prev) => prev.filter((x) => x !== r))} className="hover:text-destructive">
                ×
              </button>
            </span>
          ))}
        </div>
      )}
      {mention && (
        <MentionPopup
          workspaceId={workspaceId}
          query={mention.query}
          activeIdx={mentionActiveIdx}
          onActiveIdxChange={handleMentionActiveIdxChange}
          onFilesChange={handleMentionFilesChange}
          onSelect={selectMention}
          roles={roles}
        />
      )}
      {slashMention && slashCommands.length > 0 && (
        <SlashCommandPopup
          commands={slashCommands}
          query={slashMention.query}
          activeIdx={slashActiveIdx}
          onActiveIdxChange={handleSlashActiveIdxChange}
          onCommandsChange={handleSlashCommandsChange}
          onSelect={selectSlashCommand}
        />
      )}
      <textarea
        ref={textareaRef}
        value={input}
        onChange={handleInputChange}
        onCompositionStart={handleCompositionStart}
        onCompositionEnd={handleCompositionEnd}
        onKeyDown={(e) => {
          if (ime.isComposing(e)) return;
          if (e.key === 'Escape') {
            closeMention();
            closeSlashMention();
            return;
          }
          if (mention) {
            if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
              e.preventDefault();
              const n = mentionFiles.length;
              if (n > 0) {
                setMentionActiveIdx((prev) => (e.key === 'ArrowDown' ? (prev + 1) % n : (prev - 1 + n) % n));
              }
              return;
            }
            if (e.key === 'Tab' || (e.key === 'Enter' && !e.shiftKey)) {
              e.preventDefault();
              const target = mentionFiles[mentionActiveIdx];
              if (target) selectMention(target);
              return;
            }
          } else if (slashMention) {
            if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
              e.preventDefault();
              const n = slashFilteredCommands.length;
              if (n > 0) {
                setSlashActiveIdx((prev) => (e.key === 'ArrowDown' ? (prev + 1) % n : (prev - 1 + n) % n));
              }
              return;
            }
            if (e.key === 'Tab' || (e.key === 'Enter' && !e.shiftKey)) {
              e.preventDefault();
              const target = slashFilteredCommands[slashActiveIdx];
              if (target) selectSlashCommand(target.name);
              return;
            }
          }
          if (e.key === 'Enter' && !e.shiftKey) {
            e.preventDefault();
            onSend();
          }
        }}
        onBlur={() => {
          if (mention) blurTimerRef.current = globalThis.setTimeout(closeMention, 150);
          if (slashMention) blurTimerRef.current = globalThis.setTimeout(closeSlashMention, 150);
        }}
        onFocus={() => {
          if (blurTimerRef.current) {
            clearTimeout(blurTimerRef.current);
            blurTimerRef.current = null;
          }
        }}
        placeholder={placeholder}
        className="w-full min-h-[2.25rem] resize-none rounded-t-2xl border-0 bg-transparent px-3 pb-1 pt-2 text-base leading-5 focus:outline-none md:text-sm"
        rows={1}
      />
    </>
  );
}

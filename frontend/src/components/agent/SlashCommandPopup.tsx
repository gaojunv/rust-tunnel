import { useEffect, useMemo, useRef } from 'react';
import { useTranslation } from 'react-i18next';
import { TerminalSquare } from 'lucide-react';

export interface SlashCommand {
  name: string;
  description?: string;
}

interface Props {
  /** 全量命令列表（来自 WS available_commands 帧）。 */
  commands: SlashCommand[];
  /** / 后已输入的查询串（用于过滤）。 */
  query: string;
  /** 父组件键盘驱动的高亮下标（受控组件：↑↓ 由 textarea onKeyDown 转发）。 */
  activeIdx: number;
  /** 列表变化时把高亮回卷到首项（父组件持有 activeIdx，经此回调重置）。 */
  onActiveIdxChange: (idx: number) => void;
  /** 把可选中命令列表上报给父组件（Enter/Tab 选中依赖）。 */
  onCommandsChange: (commands: SlashCommand[]) => void;
  onSelect: (name: string) => void;
}

export default function SlashCommandPopup({
  commands,
  query,
  activeIdx,
  onActiveIdxChange,
  onCommandsChange,
  onSelect,
}: Props) {
  const { t } = useTranslation();
  const q = query.toLowerCase();

  const filtered = useMemo(
    () => commands.filter((c) => c.name.toLowerCase().includes(q)),
    [commands, q],
  );

  // 内容守卫：仅当列表内容实际变化时上报并重置高亮
  const prevRef = useRef<SlashCommand[] | null>(null);
  useEffect(() => {
    const prev = prevRef.current;
    if (
      prev !== null &&
      prev.length === filtered.length &&
      prev.every((c, i) => c.name === filtered[i].name)
    ) {
      return;
    }
    prevRef.current = filtered;
    onCommandsChange(filtered);
    onActiveIdxChange(0);
  }, [filtered, onCommandsChange, onActiveIdxChange]);

  if (filtered.length === 0) {
    return (
      <div className="absolute bottom-full left-0 mb-1 max-h-56 w-80 max-w-full overflow-y-auto rounded-lg border bg-popover shadow-lg">
        <p className="px-3 py-2 text-xs text-muted-foreground">{t('agent.noMatchingCommands')}</p>
      </div>
    );
  }

  return (
    <div className="absolute bottom-full left-0 mb-1 max-h-56 w-80 max-w-full overflow-y-auto rounded-lg border bg-popover shadow-lg">
      {filtered.map((c, i) => (
        <button
          key={c.name}
          type="button"
          onClick={() => onSelect(c.name)}
          className={`flex w-full items-center gap-2 px-3 py-1.5 text-left text-sm ${i === activeIdx ? 'bg-accent' : ''}`}
        >
          <TerminalSquare className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
          <span className="font-medium">/{c.name}</span>
          {c.description && (
            <span className="ml-auto min-w-0 shrink-0 truncate text-[11px] text-muted-foreground">
              {c.description}
            </span>
          )}
        </button>
      ))}
    </div>
  );
}

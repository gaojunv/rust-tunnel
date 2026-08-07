import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Folder, TerminalSquare, GitBranch } from 'lucide-react';
import { cn } from '@/lib/utils';
import FilesPanel from './panels/FilesPanel';
import TerminalPanel from './panels/TerminalPanel';
import GitPanel from './panels/GitPanel';

type PanelKind = 'files' | 'terminal' | 'git';

const ICONS: {
  kind: PanelKind;
  Icon: typeof Folder;
  labelKey: 'agent.files' | 'agent.terminal' | 'agent.git';
}[] = [
  { kind: 'files', Icon: Folder, labelKey: 'agent.files' },
  { kind: 'terminal', Icon: TerminalSquare, labelKey: 'agent.terminal' },
  { kind: 'git', Icon: GitBranch, labelKey: 'agent.git' },
];

/** 面板默认/最小宽度（px）：终端需要足够列宽（80 列等宽字符），文件树/git 列表用窄栏。 */
const PANEL_DEFAULT_WIDTH: Record<PanelKind, number> = {
  files: 288,
  git: 320,
  terminal: 576,
};
const PANEL_MIN_WIDTH: Record<PanelKind, number> = {
  files: 200,
  git: 220,
  terminal: 320,
};
/** 面板最大宽度：不超过外层容器宽度的 80%（至少保留对话区可见）。 */
const MAX_WIDTH_RATIO = 0.8;

interface ActivityBarProps {
  sessionId: string;
  workspaceId: string;
}

export default function ActivityBar({ sessionId, workspaceId }: ActivityBarProps) {
  const { t } = useTranslation();
  const [active, setActive] = useState<PanelKind | null>(null);
  // 每种面板各自记住拖动后的宽度（px）
  const [widths, setWidths] = useState<Record<PanelKind, number>>({ ...PANEL_DEFAULT_WIDTH });
  const [dragging, setDragging] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);

  const toggle = (kind: PanelKind) => setActive((cur) => (cur === kind ? null : kind));

  const onHandlePointerDown = useCallback(
    (e: React.PointerEvent<HTMLDivElement>) => {
      if (!active) return;
      e.preventDefault();
      const kind = active;
      const startX = e.clientX;
      const startWidth = widths[kind];
      const containerWidth =
        rootRef.current?.parentElement?.clientWidth || window.innerWidth;
      const maxWidth = Math.max(
        PANEL_MIN_WIDTH[kind],
        Math.floor(containerWidth * MAX_WIDTH_RATIO)
      );
      setDragging(true);
      const onMove = (ev: PointerEvent) => {
        const w = Math.min(
          maxWidth,
          Math.max(PANEL_MIN_WIDTH[kind], startWidth + ev.clientX - startX)
        );
        setWidths((cur) => ({ ...cur, [kind]: w }));
      };
      const onUp = () => {
        setDragging(false);
        window.removeEventListener('pointermove', onMove);
        window.removeEventListener('pointerup', onUp);
      };
      window.addEventListener('pointermove', onMove);
      window.addEventListener('pointerup', onUp);
    },
    [active, widths]
  );

  // 窗口尺寸变化时把超出上限的宽度钳回合法范围
  useEffect(() => {
    const onResize = () => {
      const containerWidth =
        rootRef.current?.parentElement?.clientWidth || window.innerWidth;
      setWidths((cur) => {
        let changed = false;
        const next = { ...cur };
        (Object.keys(next) as PanelKind[]).forEach((k) => {
          const max = Math.max(
            PANEL_MIN_WIDTH[k],
            Math.floor(containerWidth * MAX_WIDTH_RATIO)
          );
          if (next[k] > max) {
            next[k] = max;
            changed = true;
          }
        });
        return changed ? next : cur;
      });
    };
    window.addEventListener('resize', onResize);
    return () => window.removeEventListener('resize', onResize);
  }, []);

  return (
    <div ref={rootRef} className="flex h-full shrink-0">
      {/* 极窄图标栏（VS Code Activity Bar） */}
      <div className="flex w-12 flex-col items-center gap-1 border-r border-border/60 py-2">
        {ICONS.map(({ kind, Icon, labelKey }) => (
          <button
            key={kind}
            type="button"
            aria-label={t(labelKey)}
            aria-pressed={active === kind}
            title={t(labelKey)}
            onClick={() => toggle(kind)}
            className={cn(
              'flex h-9 w-9 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-accent hover:text-foreground',
              active === kind && 'bg-accent text-primary'
            )}
          >
            <Icon className="h-4 w-4" />
          </button>
        ))}
      </div>

      {/* 可展开面板（宽度可拖动，VS Code 式分隔条） */}
      {active && (
        <div
          data-testid="activity-panel"
          data-panel={active}
          role="region"
          aria-label={t('agent.activityPanel')}
          style={{ width: widths[active] }}
          className="flex min-h-0 shrink-0 flex-col overflow-hidden border-r border-border/60"
        >
          {active === 'files' && <FilesPanel workspaceId={workspaceId} />}
          {active === 'terminal' && <TerminalPanel workspaceId={workspaceId} />}
          {active === 'git' && <GitPanel sessionId={sessionId} workspaceId={workspaceId} />}
        </div>
      )}

      {/* 拖动手柄：面板展开时位于面板右缘 */}
      {active && (
        <div
          data-testid="activity-panel-resizer"
          role="separator"
          aria-orientation="vertical"
          aria-label={t('agent.resizePanel')}
          onPointerDown={onHandlePointerDown}
          className={cn(
            'w-1 shrink-0 cursor-col-resize transition-colors hover:bg-primary/40',
            dragging ? 'bg-primary/60' : 'bg-transparent'
          )}
        />
      )}

      {/* 拖动中禁用指针事件与文本选择，避免 iframe/终端吞掉 pointermove */}
      {dragging && (
        <div
          className="fixed inset-0 z-50 cursor-col-resize select-none"
          data-testid="activity-panel-drag-overlay"
        />
      )}
    </div>
  );
}

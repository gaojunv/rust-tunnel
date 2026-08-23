import { memo, useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Folder, TerminalSquare, GitBranch, Workflow } from 'lucide-react';
import { cn } from '@/lib/utils';
import { Sheet, SheetContent, SheetTitle } from '@/components/ui/sheet';
import FilesPanel from './panels/FilesPanel';
import TerminalPanel from './panels/TerminalPanel';
import GitPanel from './panels/GitPanel';
import GitHubActionsPanel from './panels/github/GitHubActionsPanel';
import { safeLocalStorageGet, safeLocalStorageSet } from './safeStorage';

type PanelKind = 'files' | 'terminal' | 'git' | 'github';

const ICONS: {
  kind: PanelKind;
  Icon: typeof Folder;
  labelKey: 'agent.files' | 'agent.terminal' | 'agent.git' | 'agent.github';
}[] = [
  { kind: 'files', Icon: Folder, labelKey: 'agent.files' },
  { kind: 'terminal', Icon: TerminalSquare, labelKey: 'agent.terminal' },
  { kind: 'git', Icon: GitBranch, labelKey: 'agent.git' },
  { kind: 'github', Icon: Workflow, labelKey: 'agent.github' },
];

/** 面板默认/最小宽度（px）：终端需要足够列宽（80 列等宽字符），文件树/git 列表用窄栏。 */
const PANEL_DEFAULT_WIDTH: Record<PanelKind, number> = {
  files: 288,
  git: 320,
  terminal: 576,
  github: 340,
};
const PANEL_MIN_WIDTH: Record<PanelKind, number> = {
  files: 200,
  git: 220,
  terminal: 320,
  github: 240,
};
/** 面板最大宽度：不超过外层容器宽度的 80%（至少保留对话区可见）。 */
const MAX_WIDTH_RATIO = 0.8;

/** 移动端底部 Sheet 高度比例持久化 */
const MOBILE_PANEL_HEIGHT_KEY = 'agent.mobilePanelHeightRatio';
const MOBILE_PANEL_DEFAULT_RATIO = 0.5;
const MOBILE_PANEL_MIN_RATIO = 0.25;
const MOBILE_PANEL_MAX_RATIO = 0.92;
const MOBILE_PANEL_STEP = 0.05;
const clampRatio = (v: number) =>
  Math.min(MOBILE_PANEL_MAX_RATIO, Math.max(MOBILE_PANEL_MIN_RATIO, v));

interface ActivityBarProps {
  sessionId: string;
  workspaceId: string;
  /**
   * 'sidebar'：桌面端 VS Code 式侧栏（默认，向后兼容）；
   * 'mobile'：底部固定图标栏 + 底部 Sheet 面板（<768px）。
   */
  variant?: 'sidebar' | 'mobile';
}

function ActivityBar({ sessionId, workspaceId, variant = 'sidebar' }: ActivityBarProps) {
  const { t } = useTranslation();
  const [active, setActive] = useState<PanelKind | null>(null);
  // 每种面板各自记住拖动后的宽度（px）
  const [widths, setWidths] = useState<Record<PanelKind, number>>({ ...PANEL_DEFAULT_WIDTH });
  const [dragging, setDragging] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  // 移动端底部 Sheet 共享高度比例（0.25-0.92，默认 0.5 半屏）
  const [heightRatio, setHeightRatio] = useState(MOBILE_PANEL_DEFAULT_RATIO);
  const [mobileDragging, setMobileDragging] = useState(false);

  // 读取持久化高度：校验数字并钳到合法区间，非法回落 0.5
  useEffect(() => {
    const raw = safeLocalStorageGet(MOBILE_PANEL_HEIGHT_KEY);
    if (raw == null) return;
    const n = Number(raw);
    if (!Number.isFinite(n)) return;
    setHeightRatio(clampRatio(n));
  }, []);

  // 高度变化时写回 localStorage
  useEffect(() => {
    safeLocalStorageSet(MOBILE_PANEL_HEIGHT_KEY, String(heightRatio));
  }, [heightRatio]);

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

  // 移动端 Sheet 顶部拖动手柄：纵向拖动改变共享高度比例
  const onMobileHandlePointerDown = useCallback(
    (e: React.PointerEvent<HTMLDivElement>) => {
      e.preventDefault();
      const startY = e.clientY;
      const startRatio = heightRatio;
      setMobileDragging(true);
      const onMove = (ev: PointerEvent) => {
        const deltaY = ev.clientY - startY;
        const vh = window.innerHeight || 1;
        // 向上拖（deltaY 为负）→ 变高，故用减法
        const next = clampRatio(startRatio - deltaY / vh);
        setHeightRatio(next);
      };
      const onUp = () => {
        setMobileDragging(false);
        window.removeEventListener('pointermove', onMove);
        window.removeEventListener('pointerup', onUp);
      };
      window.addEventListener('pointermove', onMove);
      window.addEventListener('pointerup', onUp);
    },
    [heightRatio]
  );

  const onMobileHandleKeyDown = useCallback((e: React.KeyboardEvent<HTMLDivElement>) => {
    if (e.key === 'ArrowUp') {
      e.preventDefault();
      setHeightRatio((r) => clampRatio(r + MOBILE_PANEL_STEP));
    } else if (e.key === 'ArrowDown') {
      e.preventDefault();
      setHeightRatio((r) => clampRatio(r - MOBILE_PANEL_STEP));
    }
  }, []);

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

  // 移动端（<768px）：VS Code 侧栏在 393px 宽度上不可用——改为卡片内底部 footer
  // 图标行（由 AgentPage 渲染在对话区下方），面板经底部 Sheet（side="bottom"）弹出。
  // 面板内容仅在对应 Sheet open 时挂载（Radix Dialog 关闭即卸载），避免在页面常驻
  // 重量级文件/终端面板。Sheet 默认半屏可拖动，四面板共享同一 heightRatio。
  if (variant === 'mobile') {
    return (
      <>
        {/* 底栏 footer：与顶栏（border-b p-1.5 + ghost sm 按钮）上下对称——
            相同行高/内边距/按钮规格（h-9 w-9 + h-4 图标），border-t 贴住对话区，
            无空隙；justify-around 让四个按钮整行平均分布。
            安全区垫高由 AppLayout 容器统一承担，本栏不重复处理。 */}
        <div className="flex items-center justify-around border-t border-border/60 p-1.5">
          {ICONS.map(({ kind, Icon, labelKey }) => (
            <button
              key={kind}
              type="button"
              aria-label={t(labelKey)}
              aria-pressed={active === kind}
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
        {/* 底部 Sheet 面板：每个图标一个受控 Sheet，open 时挂载对应面板 */}
        {ICONS.map(({ kind, labelKey }) => (
          <Sheet
            key={kind}
            open={active === kind}
            onOpenChange={(open) => setActive(open ? kind : null)}
          >
            {/* 默认半屏（50dvh），可拖动改变高度；保留 dvh 语义，地址栏伸缩时不跳变 */}
            <SheetContent
              side="bottom"
              className="flex flex-col gap-0 p-0"
              style={{ height: `calc(${heightRatio} * 100dvh)` }}
            >
              {/* 顶部拖动手柄：iOS 式 grabber，py-2 撑出约 28px 触控区 */}
              <div
                role="separator"
                aria-orientation="horizontal"
                /* key 由主会话统一补到 locales JSON，此处先硬编码（as any 规避已生成的严格 key 类型） */
                aria-label={(t as (k: string) => string)('agent.resizePanelHeight')}
                tabIndex={0}
                onPointerDown={onMobileHandlePointerDown}
                onKeyDown={onMobileHandleKeyDown}
                className="flex touch-none justify-center py-2"
              >
                <div className="h-1 w-10 rounded-full bg-muted-foreground/30" />
              </div>
              <div className="flex items-center justify-between border-b border-border/60 py-3 pl-4 pr-10">
                <SheetTitle className="text-sm font-medium">{t(labelKey)}</SheetTitle>
              </div>
              <div className="min-h-0 flex-1 overflow-hidden">
                {kind === 'files' && <FilesPanel workspaceId={workspaceId} />}
                {kind === 'terminal' && <TerminalPanel workspaceId={workspaceId} />}
                {kind === 'git' && <GitPanel sessionId={sessionId} workspaceId={workspaceId} />}
                {kind === 'github' && <GitHubActionsPanel workspaceId={workspaceId} />}
              </div>
            </SheetContent>
          </Sheet>
        ))}
        {/* 拖动期透明遮罩：防底层吞掉 pointermove，复用桌面端手法 */}
        {mobileDragging && (
          <div
            className="fixed inset-0 z-50 cursor-row-resize select-none"
            data-testid="activity-panel-drag-overlay"
          />
        )}
      </>
    );
  }

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
          {active === 'github' && <GitHubActionsPanel workspaceId={workspaceId} />}
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

export default memo(ActivityBar);

// VS Code 风格面板统一样式基线
// 文本：主 text-xs，次要 text-[11px]；行高：py-1 + min-h-[36px] 移动端 / md:min-h-[28px] 桌面
// 注意 cn() 的 tailwind-merge 会吞同组并列类，同组只留一个（故行高/字号各仅一处定义）

export const PANEL_ROW =
  'flex items-center gap-1.5 min-h-[36px] md:min-h-[28px] rounded px-1.5 py-1 text-xs hover:bg-accent/50';

export const PANEL_ROW_DENSE =
  'flex items-center gap-1 min-h-[36px] md:min-h-[28px] rounded px-1 py-1 text-xs hover:bg-accent/50';

export const PANEL_SUBTEXT = 'text-[11px] text-muted-foreground';

export const PANEL_SUBTEXT_SMALL = 'text-[11px] text-muted-foreground/70';

export const PANEL_ICON_BUTTON =
  'shrink-0 rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground';

export const PANEL_ACTION_VISIBLE =
  'opacity-100 md:opacity-0 md:group-hover:opacity-100 transition-opacity';

export const PANEL_GROUP_HEADER =
  'group flex items-center gap-1 rounded px-1 py-1 hover:bg-accent/50';

export const PANEL_TAB_BAR = 'flex border-b border-border/60 px-1';

export const PANEL_TAB = 'flex-1 rounded-t px-1 py-1 text-xs font-medium text-muted-foreground transition-colors hover:text-foreground';

export const PANEL_TAB_ACTIVE = 'border-b-2 border-primary bg-accent/40 text-foreground';

/**
 * Agent 底部模式切换条 —— 三档分段控件（只读/自动/全权）
 * 受控组件：当前档由父级（AgentPanel）持有；切换只上报 onModeChange。
 *
 * 语义：
 * - 切到「全权」首次弹一次内联确认，确认后写入 localStorage 记住，不再骚扰。
 * - `disabled`（回合运行中）由父级决定是否允许切换：本组件不拦截点击，
 *   仅在 disabled 时展示「下一回合生效」提示——生效时机归 2E 接线控制。
 * - `modelSlot` 预留插槽：后续 2E 把模型选择器挂进来（本文件不实现）。
 * - 档位 → per-turn `approvalPolicy`/`sandboxPolicy` 的映射在
 *   `lib/agent/mode-presets.ts`（modeToTurnOverrides），由 2E 在 turnStart 时套用。
 */
import { useMemo, useState } from "react";
import { Eye, ShieldCheck, Zap, TriangleAlert } from "lucide-react";
import { AGENT_MODES, MODE_PRESETS, isAgentMode } from "@/lib/agent/mode-presets";
import type { ModePreset } from "@/lib/agent/mode-presets";

const FULL_CONFIRM_KEY = "wiki.agent.mode.fullConfirmed.v1";

function readFullConfirmed(): boolean {
  try {
    return localStorage.getItem(FULL_CONFIRM_KEY) === "1";
  } catch {
    return false;
  }
}

function writeFullConfirmed(): void {
  try {
    localStorage.setItem(FULL_CONFIRM_KEY, "1");
  } catch {
    // 隐私模式等忽略：仅本次会话内不再弹
  }
}

type ModeBarProps = {
  /** 当前档位（store 回读可能为脏字符串，非法时按默认档展示） */
  mode: string;
  /** 切换上报：只传合法档位 key */
  onModeChange: (mode: string) => void;
  /** vault 根路径：供「自动」档说明展示 */
  vaultRoot: string;
  /** 预留模型选择器插槽（2E 挂载） */
  modelSlot?: React.ReactNode;
  /** 回合运行中：仅影响切换生效提示，不拦截点击 */
  disabled?: boolean;
};

const MODE_ICONS = {
  readonly: Eye,
  auto: ShieldCheck,
  full: Zap,
} as const;

/** 当前档的激活样式：全权标红警示 */
function activeClass(preset: ModePreset): string {
  switch (preset.danger) {
    case "high":
      return "bg-red-500/15 text-red-600 dark:text-red-400 ring-1 ring-inset ring-red-500/40";
    case "medium":
      return "bg-amber-500/15 text-amber-700 dark:text-amber-400 ring-1 ring-inset ring-amber-500/40";
    default:
      return "bg-foreground/10 text-foreground ring-1 ring-inset ring-foreground/10";
  }
}

export function ModeBar({ mode, onModeChange, vaultRoot, modelSlot, disabled }: ModeBarProps) {
  // localStorage 记住是否确认过全权
  const [fullConfirmed, setFullConfirmed] = useState<boolean>(() => readFullConfirmed());
  // 待确认的全权切换（弹一次内联确认）
  const [pendingFull, setPendingFull] = useState(false);

  const activeMode = useMemo(() => (isAgentMode(mode) ? mode : "auto"), [mode]);
  const activePreset = MODE_PRESETS[activeMode];

  const requestChange = (next: string) => {
    if (next === activeMode) return;
    if (next === "full" && !fullConfirmed) {
      setPendingFull(true);
      return;
    }
    onModeChange(next);
  };

  const confirmFull = () => {
    setFullConfirmed(true);
    writeFullConfirmed();
    setPendingFull(false);
    onModeChange("full");
  };

  const cancelFull = () => setPendingFull(false);

  return (
    <div className="space-y-1.5">
      {/* 全权首次确认（内联警示，不打断队列） */}
      {pendingFull && (
        <div className="flex flex-wrap items-center gap-2 rounded-md border border-red-500/40 bg-red-500/10 px-2.5 py-1.5">
          <TriangleAlert className="size-3.5 shrink-0 text-red-600 dark:text-red-400" />
          <p className="min-w-0 flex-1 text-[11px] leading-snug text-red-700 dark:text-red-300">
            切到「全权」会跳过全部审批并可访问整个系统，仅当你完全信任本次任务时使用。
          </p>
          <div className="flex shrink-0 gap-1.5">
            <button
              type="button"
              onClick={confirmFull}
              className="rounded bg-red-600 px-2 py-0.5 text-[11px] font-medium text-white hover:bg-red-700"
            >
              确认切换
            </button>
            <button
              type="button"
              onClick={cancelFull}
              className="rounded bg-background px-2 py-0.5 text-[11px] text-muted-foreground ring-1 ring-inset ring-border hover:text-foreground"
            >
              取消
            </button>
          </div>
        </div>
      )}

      <div className="flex items-center gap-1.5 rounded-lg border border-border/60 bg-background/60 p-0.5">
        <div className="flex min-w-0 flex-1">
          {AGENT_MODES.map((m) => {
            const preset = MODE_PRESETS[m];
            const isActive = m === activeMode;
            const Icon = MODE_ICONS[m];
            const high = m === "full";
            return (
              <button
                key={m}
                type="button"
                aria-pressed={isActive}
                onClick={() => requestChange(m)}
                title={preset.description}
                className={`flex min-w-0 flex-1 items-center justify-center gap-1 rounded-md px-1.5 py-1 text-[11px] transition-colors ${
                  isActive
                    ? activeClass(preset)
                    : high
                      ? "text-muted-foreground hover:bg-red-500/10 hover:text-red-600 dark:hover:text-red-400"
                      : "text-muted-foreground hover:bg-accent hover:text-foreground"
                }`}
              >
                <Icon className="size-3.5 shrink-0" />
                <span className="truncate">{preset.label}</span>
              </button>
            );
          })}
        </div>
        {modelSlot && <div className="shrink-0 pl-0.5">{modelSlot}</div>}
      </div>

      {/* 档位说明：一句话 + 生效时机 */}
      <div className="flex min-w-0 items-baseline gap-2 px-1">
        <p className="min-w-0 flex-1 truncate text-[11px] text-muted-foreground" title={activePreset.description}>
          {activeMode === "auto"
            ? `${activePreset.description} 可写根：${vaultRoot || "（未设置 vault）"}`
            : activePreset.description}
        </p>
        {disabled && <span className="shrink-0 text-[10px] text-amber-600/90 dark:text-amber-400/90">运行中，切换于下一回合生效</span>}
      </div>
    </div>
  );
}

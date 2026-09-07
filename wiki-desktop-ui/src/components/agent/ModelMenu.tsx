/**
 * ModelMenu —— 模型选择弹出菜单（含推理强度二级区）。
 * 由 AgentPanel（2E 接线）渲染在触发按钮的 relative 容器内，面板内绝对定位（不引 portal）。
 * 定位默认「贴着容器上沿向上展开」（bottom-full），如容器在下侧，可用 className 覆盖为 top-full。
 *
 * 强度二级区策略：
 * - 悬停某模型行且该模型带 efforts 时，在菜单底部展开其强度区；
 *   未悬停时展开「当前选中模型」的强度区（若有）。
 * - 模型不带 efforts（gateway 无元数据）时完全不渲染强度区。
 * - "默认（跟随模型）"项 = effort null；点击具体强度项 = 指定该 effort。
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { cn } from "@/lib/utils";
import type { EffortOption, ModelOption } from "@/lib/agent/models";

export type ModelMenuProps = {
  models: ModelOption[];
  /** 当前选中模型 id（可能不在列表中） */
  value: string | null;
  /** 当前推理强度；null = 跟随模型默认 */
  effort?: string | null;
  onSelectModel: (id: string) => void;
  onSelectEffort: (effort: string | null) => void;
  /** 可选：点击外部 / Esc 关闭；不传则菜单常驻（由上层卸载） */
  onClose?: () => void;
  /** 可选定位/宽度覆盖，如 "w-64 top-full mt-1" */
  className?: string;
};

/** 常见 codex 强度值 → 中文标签；未知值原样展示 */
const EFFORT_LABELS: Record<string, string> = {
  minimal: "最低",
  low: "低",
  medium: "中",
  high: "高",
};

function effortLabel(effort: string): string {
  return EFFORT_LABELS[effort] ?? effort;
}

function CheckDot({ active }: { active: boolean }) {
  if (!active) return <span className="w-3 shrink-0" aria-hidden="true" />;
  return (
    <span className="w-3 shrink-0 text-primary" aria-hidden="true">
      ✓
    </span>
  );
}

export function ModelMenu({
  models,
  value,
  effort = null,
  onSelectModel,
  onSelectEffort,
  onClose,
  className,
}: ModelMenuProps) {
  const rootRef = useRef<HTMLDivElement>(null);
  const [hoveredId, setHoveredId] = useState<string | null>(null);

  // 点击外部 / Esc 关闭（onClose 存在时）
  useEffect(() => {
    if (!onClose) return;
    const onPointerDown = (e: Event) => {
      const node = rootRef.current;
      if (!node) return;
      const t = e.target;
      if (t instanceof Node && node.contains(t)) return;
      onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("pointerdown", onPointerDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("pointerdown", onPointerDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [onClose]);

  // 强度区对应模型：优先悬停项，否则当前选中项
  const activeModel = useMemo<ModelOption | null>(() => {
    if (hoveredId) {
      const h = models.find((m) => m.id === hoveredId);
      if (h) return h;
    }
    return models.find((m) => m.id === value) ?? null;
  }, [models, hoveredId, value]);

  const efforts: EffortOption[] =
    activeModel && activeModel.efforts && activeModel.efforts.length > 0
      ? activeModel.efforts
      : [];
  const defaultEffort = activeModel?.defaultEffort ?? null;
  const defaultEffortDesc = defaultEffort
    ? efforts.find((e) => e.effort === defaultEffort)?.description
    : undefined;

  const handleSelectModel = (id: string) => {
    onSelectModel(id);
    const chosen = models.find((m) => m.id === id);
    // 选中模型无强度选项则顺手关闭（onClose 存在时），否则停留以便选择强度
    if (onClose && chosen && (!chosen.efforts || chosen.efforts.length === 0)) onClose();
  };

  return (
    <div
      ref={rootRef}
      className={cn(
        "absolute bottom-full left-0 right-0 z-10 mb-1 flex max-h-72 flex-col overflow-hidden rounded-lg border border-border bg-popover text-xs shadow-xl",
        className,
      )}
    >
      {/* 模型列表 */}
      <div className="min-h-0 flex-1 overflow-y-auto p-1.5">
        {models.length === 0 && (
          <p className="px-2.5 py-2 text-muted-foreground">无可用模型</p>
        )}
        {models.map((m) => {
          const active = m.id === value;
          const hovered = m.id === hoveredId;
          return (
            <button
              key={m.id}
              type="button"
              onMouseEnter={() => setHoveredId(m.id)}
              onMouseLeave={() => setHoveredId((h) => (h === m.id ? null : h))}
              onClick={() => handleSelectModel(m.id)}
              className={cn(
                "flex w-full items-center gap-2 rounded-md px-2.5 py-1.5 text-left transition-colors",
                active || hovered ? "bg-accent" : "hover:bg-accent/60",
              )}
              title={m.id}
            >
              <CheckDot active={active} />
              <span className="min-w-0 flex-1 truncate">{m.label}</span>
              {m.efforts && m.efforts.length > 0 && (
                <span className="shrink-0 text-[10px] text-muted-foreground">强度</span>
              )}
            </button>
          );
        })}
      </div>

      {/* 推理强度二级区：仅当模型带 efforts 时出现 */}
      {efforts.length > 0 && (
        <div className="border-t border-border/60 bg-background/40">
          <div className="flex items-center gap-1 px-3 pb-0.5 pt-1.5 text-[10px] text-muted-foreground">
            <span className="min-w-0 flex-1 truncate">推理强度 · {activeModel?.label}</span>
            {defaultEffort && (
              <span className="shrink-0">默认 {effortLabel(defaultEffort)}</span>
            )}
          </div>
          <div className="flex flex-col p-1.5">
            {/* 跟随模型默认 */}
            <button
              type="button"
              onClick={() => onSelectEffort(null)}
              className={cn(
                "flex w-full items-start gap-2 rounded-md px-2.5 py-1 text-left transition-colors",
                effort == null ? "bg-accent" : "hover:bg-accent/60",
              )}
            >
              <CheckDot active={effort == null} />
              <span className="min-w-0">
                <span className="block">跟随默认</span>
                {defaultEffortDesc && (
                  <span className="block truncate text-muted-foreground">{defaultEffortDesc}</span>
                )}
              </span>
            </button>
            {efforts.map((opt) => {
              const active = effort === opt.effort;
              return (
                <button
                  key={opt.effort}
                  type="button"
                  onClick={() => onSelectEffort(opt.effort)}
                  className={cn(
                    "flex w-full items-start gap-2 rounded-md px-2.5 py-1 text-left transition-colors",
                    active ? "bg-accent" : "hover:bg-accent/60",
                  )}
                >
                  <CheckDot active={active} />
                  <span className="min-w-0">
                    <span className="block">
                      {effortLabel(opt.effort)}
                      <span className="ml-1 text-[10px] text-muted-foreground">{opt.effort}</span>
                    </span>
                    {opt.description && (
                      <span className="block truncate text-muted-foreground">{opt.description}</span>
                    )}
                  </span>
                </button>
              );
            })}
          </div>
        </div>
      )}
    </div>
  );
}

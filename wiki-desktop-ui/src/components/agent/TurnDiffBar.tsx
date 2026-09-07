/**
 * TurnDiffBar —— turn 级聚合 diff 审查条（2A'）
 * 输入框上方：turn 完成且有聚合 diff（view.turnDiffs）时展示
 * 「变更 N 文件 +x/-y ｜ 查看 ｜ 接受 ｜ 拒绝」。
 *
 * 受控组件：父级（2E 接线 AgentPanel）持有 turnDiffs / dismissed / 审查弹层开关；
 * 本组件只负责统计与回调透传。无 diff 时渲染 null（2E 可直接条件挂载）。
 */
import { Button } from "@/components/ui/button";
import { parseAggregatedDiff, summarizePatches } from "@/lib/agent/diff-review";
import type { TurnDiffView } from "@/lib/agent/codec";

export type TurnDiffBarProps = {
  /** 聚合 diff 快照（codec TurnDiffView，与 2E AgentPanel 中 view.turnDiffs 同源） */
  turnDiffs: TurnDiffView[];
  /** 当前选中（或最近）turn 的 id：存在则展示该 turn，无则展示最后一个有 diff 的 turn */
  turnId?: string | null;
  /** 该 turn 是否已审查完毕（接受/拒绝后父级置 true → 隐藏本条；重置见 on reopen） */
  dismissed?: boolean;
  /** 点「查看」→ 打开全屏审查弹层（DiffReviewDialog） */
  onReview: (turnId: string) => void;
  /** 点「接受」→ 清除审查态（文件已落盘，无需再动） */
  onAccept: (turnId: string) => void;
  /** 点「拒绝」→ 打开审查弹层并预选拒绝流程（等价 onReview + 拒绝态） */
  onReject: (turnId: string) => void;
};

export function TurnDiffBar({
  turnDiffs,
  turnId,
  dismissed = false,
  onReview,
  onAccept,
  onReject,
}: TurnDiffBarProps) {
  // 选定展示的 turn：指定 id 优先，否则最后一个有 diff 的 turn
  const active =
    (turnId ? turnDiffs.find((t) => t.turnId === turnId) : undefined) ??
    turnDiffs[turnDiffs.length - 1];
  if (!active || dismissed) return null;

  // 统计（解析失败/畸形 → 回退为原始文本行扫，避免弹层与条统计不一致由弹层另行降级）
  let summary: { files: number; adds: number; dels: number } | null = null;
  try {
    summary = summarizePatches(parseAggregatedDiff(active.diff));
  } catch {
    summary = null;
  }

  return (
    <div
      className="flex w-full items-center gap-2 border-y border-border/40 bg-card/40 px-1 py-0.5 text-xs"
      data-testid="turn-diff-bar"
      role="status"
      aria-label="本回合文件变更审查"
    >
      <span className="shrink-0 select-none font-mono text-muted-foreground">•</span>
      <span className="font-mono text-muted-foreground">diff</span>
      {summary ? (
        <span className="min-w-0 flex-1 truncate">
          变更 {summary.files} 文件
          <span className="ml-1.5 font-mono text-[11px]">
            <span className="text-emerald-600 dark:text-emerald-400">+{summary.adds}</span>
            <span className="mx-0.5 text-muted-foreground">·</span>
            <span className="text-red-600 dark:text-red-400">-{summary.dels}</span>
          </span>
        </span>
      ) : (
        <span className="min-w-0 flex-1 truncate text-muted-foreground">
          本回合有文件变更（diff 不可解析，可查看原文）
        </span>
      )}
      <span className="ml-auto flex shrink-0 items-center gap-1">
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="h-6 px-2 text-[11px] text-muted-foreground hover:text-foreground"
          onClick={() => onReview(active.turnId)}
        >
          查看
        </Button>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="h-6 px-2 text-[11px] text-emerald-600 hover:text-emerald-700 dark:text-emerald-400"
          onClick={() => onAccept(active.turnId)}
        >
          接受
        </Button>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="h-6 px-2 text-[11px] text-red-600 hover:text-red-700 dark:text-red-400"
          onClick={() => onReject(active.turnId)}
        >
          拒绝
        </Button>
      </span>
    </div>
  );
}

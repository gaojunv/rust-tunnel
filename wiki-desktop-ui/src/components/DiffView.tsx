import { buildDiffRows } from "@/lib/diff-rows";

type Props = {
  localText: string;
  remoteText: string;
  localLabel?: string;
  remoteLabel?: string;
};

export function DiffView({ localText, remoteText, localLabel = "本地", remoteLabel = "远端" }: Props) {
  if (localText === remoteText) {
    return (
      <div className="rounded border bg-muted/30 px-4 py-6 text-center text-sm text-muted-foreground">
        内容一致，无差异
      </div>
    );
  }
  const rows = buildDiffRows(localText, remoteText);
  return (
    <div className="overflow-auto rounded border">
      <div className="min-w-[520px]">
        <div className="sticky top-0 z-[1] grid grid-cols-2 border-b bg-muted text-xs font-medium">
          <div className="border-r px-2 py-1">{localLabel}</div>
          <div className="px-2 py-1">{remoteLabel}</div>
        </div>
        <div className="divide-y divide-border/50">
          {rows.map((r, idx) => {
            const leftBg =
              r.type === "del" ? "bg-red-500/15" : r.type === "pair" ? "bg-red-500/10" : "";
            const rightBg =
              r.type === "add" ? "bg-green-500/15" : r.type === "pair" ? "bg-green-500/10" : "";
            return (
              <div key={idx} className="grid grid-cols-2 text-xs font-mono">
                <div className={`flex gap-1 border-r px-1 py-0.5 ${leftBg}`}>
                  <span className="w-6 shrink-0 select-none text-right text-[10px] text-muted-foreground">
                    {r.leftNo ?? ""}
                  </span>
                  <span className="min-w-0 flex-1 whitespace-pre-wrap break-all">
                    {r.left ?? (r.type === "add" ? "" : "")}
                  </span>
                </div>
                <div className={`flex gap-1 px-1 py-0.5 ${rightBg}`}>
                  <span className="w-6 shrink-0 select-none text-right text-[10px] text-muted-foreground">
                    {r.rightNo ?? ""}
                  </span>
                  <span className="min-w-0 flex-1 whitespace-pre-wrap break-all">
                    {r.right ?? (r.type === "del" ? "" : "")}
                  </span>
                </div>
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}

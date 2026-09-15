/**
 * 右栏 Tab 容器 —— graph / backlinks / toc 按需渲染（切 tab 才挂载），
 * 仅 agent 面板 keepMounted（内部会话/流状态卸载即丢，且重量级初始化不宜反复重建）。
 * agent 面板内部自滚：容器 h-full flex flex-col，无外层与内部双层滚动
 */
import { memo, useCallback, useEffect, useState } from "react";
import { Network, Link2, List, Bot } from "lucide-react";
import { ScrollArea } from "@/components/ui/scroll-area";

type Props = {
  graphPanel: React.ReactNode;
  backlinksPanel: React.ReactNode;
  tocPanel: React.ReactNode;
  agentPanel: React.ReactNode;
};

const STORAGE_KEY = "wiki.rightpanel.tab.v1";
type TabId = "graph" | "backlinks" | "toc" | "agent";

function loadTab(): TabId {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (
      raw === "graph" ||
      raw === "backlinks" ||
      raw === "toc" ||
      raw === "agent"
    )
      return raw as TabId;
  } catch {
    // 忽略
  }
  return "graph";
}

function TabButton({
  id,
  active,
  onSelect,
  label,
  icon,
}: {
  id: TabId;
  active: boolean;
  onSelect: (id: TabId) => void;
  label: string;
  icon: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={() => onSelect(id)}
      className={`inline-flex items-center gap-1.5 rounded-md px-2.5 py-1.5 text-xs font-medium transition-colors ${
        active ? "bg-accent text-accent-foreground" : "text-muted-foreground hover:bg-accent/50 hover:text-foreground"
      }`}
      aria-label={label}
      title={label}
      aria-pressed={active}
    >
      {icon}
      {label}
    </button>
  );
}

const MemoTabButton = memo(TabButton);

export const RightPanel = memo(function RightPanel({ graphPanel, backlinksPanel, tocPanel, agentPanel }: Props) {
  const [tab, setTab] = useState<TabId>(() => loadTab());
  // agent 面板一旦挂载过就保持挂载（内部会话/流状态保留）；其余面板按 tab 按需渲染
  const [agentMounted, setAgentMounted] = useState(() => loadTab() === "agent");

  useEffect(() => {
    try {
      localStorage.setItem(STORAGE_KEY, tab);
    } catch {
      // 忽略
    }
    if (tab === "agent") setAgentMounted(true);
  }, [tab]);

  const handleSelect = useCallback((id: TabId) => setTab(id), []);

  return (
    <div className="flex h-full flex-col">
      <div className="flex items-center gap-1 border-b border-border/50 px-2 py-1.5">
        <MemoTabButton id="graph" active={tab === "graph"} onSelect={handleSelect} label="图谱" icon={<Network className="size-3.5" />} />
        <MemoTabButton id="backlinks" active={tab === "backlinks"} onSelect={handleSelect} label="反链" icon={<Link2 className="size-3.5" />} />
        <MemoTabButton id="toc" active={tab === "toc"} onSelect={handleSelect} label="大纲" icon={<List className="size-3.5" />} />
        <MemoTabButton id="agent" active={tab === "agent"} onSelect={handleSelect} label="Agent" icon={<Bot className="size-3.5" />} />
      </div>
      <div className="min-h-0 flex-1 overflow-hidden">
        {tab === "graph" && (
          <div className="h-full">
            <ScrollArea className="h-full">{graphPanel}</ScrollArea>
          </div>
        )}
        {tab === "backlinks" && (
          <div className="h-full">
            <ScrollArea className="h-full">{backlinksPanel}</ScrollArea>
          </div>
        )}
        {tab === "toc" && (
          <div className="h-full">
            <ScrollArea className="h-full">{tocPanel}</ScrollArea>
          </div>
        )}
        <div className={tab === "agent" ? "flex h-full flex-col overflow-hidden" : "hidden"}>
          {agentMounted ? agentPanel : null}
        </div>
      </div>
    </div>
  );
});

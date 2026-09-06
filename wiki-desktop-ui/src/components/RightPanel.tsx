/**
 * 右栏 Tab 容器 —— graph / backlinks / toc / agent 四面板，保持挂载以避免重复拉取与状态丢失
 */
import { useEffect, useState } from "react";
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

export function RightPanel({ graphPanel, backlinksPanel, tocPanel, agentPanel }: Props) {
  const [tab, setTab] = useState<TabId>(() => loadTab());

  useEffect(() => {
    try {
      localStorage.setItem(STORAGE_KEY, tab);
    } catch {
      // 忽略
    }
  }, [tab]);

  return (
    <div className="flex h-full flex-col">
      <div className="flex items-center gap-1 border-b border-border/60 px-2 py-1.5">
        <button
          type="button"
          onClick={() => setTab("graph")}
          className={`inline-flex items-center gap-1.5 rounded-md px-2.5 py-1.5 text-xs font-medium transition-colors ${
            tab === "graph" ? "bg-accent text-accent-foreground" : "text-muted-foreground hover:bg-accent/50 hover:text-foreground"
          }`}
          aria-label="图谱"
          title="图谱"
          aria-pressed={tab === "graph"}
        >
          <Network className="size-3.5" />
          图谱
        </button>
        <button
          type="button"
          onClick={() => setTab("backlinks")}
          className={`inline-flex items-center gap-1.5 rounded-md px-2.5 py-1.5 text-xs font-medium transition-colors ${
            tab === "backlinks" ? "bg-accent text-accent-foreground" : "text-muted-foreground hover:bg-accent/50 hover:text-foreground"
          }`}
          aria-label="反链"
          title="反链"
          aria-pressed={tab === "backlinks"}
        >
          <Link2 className="size-3.5" />
          反链
        </button>
        <button
          type="button"
          onClick={() => setTab("toc")}
          className={`inline-flex items-center gap-1.5 rounded-md px-2.5 py-1.5 text-xs font-medium transition-colors ${
            tab === "toc" ? "bg-accent text-accent-foreground" : "text-muted-foreground hover:bg-accent/50 hover:text-foreground"
          }`}
          aria-label="大纲"
          title="大纲"
          aria-pressed={tab === "toc"}
        >
          <List className="size-3.5" />
          大纲
        </button>
        <button
          type="button"
          onClick={() => setTab("agent")}
          className={`inline-flex items-center gap-1.5 rounded-md px-2.5 py-1.5 text-xs font-medium transition-colors ${
            tab === "agent" ? "bg-accent text-accent-foreground" : "text-muted-foreground hover:bg-accent/50 hover:text-foreground"
          }`}
          aria-label="Agent"
          title="Agent"
          aria-pressed={tab === "agent"}
        >
          <Bot className="size-3.5" />
          Agent
        </button>
      </div>
      <div className="min-h-0 flex-1 overflow-hidden">
        <div className={tab === "graph" ? "h-full" : "hidden"}>
          <ScrollArea className="h-full">{graphPanel}</ScrollArea>
        </div>
        <div className={tab === "backlinks" ? "h-full" : "hidden"}>
          <ScrollArea className="h-full">{backlinksPanel}</ScrollArea>
        </div>
        <div className={tab === "toc" ? "h-full" : "hidden"}>
          <ScrollArea className="h-full">{tocPanel}</ScrollArea>
        </div>
        <div className={tab === "agent" ? "h-full" : "hidden"}>
          <ScrollArea className="h-full">{agentPanel}</ScrollArea>
        </div>
      </div>
    </div>
  );
}

import { EditorView } from "@codemirror/view";
import {
  Bold,
  Italic,
  Strikethrough,
  Code,
  Heading1,
  Heading2,
  Heading3,
  List,
  ListOrdered,
  ListChecks,
  Table,
  Link,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import {
  toggleBold,
  toggleItalic,
  toggleStrikethrough,
  toggleInlineCode,
  toggleHeading,
  toggleBulletList,
  toggleOrderedList,
  toggleTaskList,
  insertTable,
  insertLink,
} from "@/lib/codemirror/format-commands";

type Props = {
  getView: () => EditorView | null;
  className?: string;
};

type Tool = {
  label: string;
  icon: React.ReactNode;
  title: string;
  run: (view: EditorView) => boolean;
};

export function EditorToolbar({ getView, className }: Props) {
  const tools: Tool[] = [
    { label: "Bold", icon: <Bold className="h-4 w-4" />, title: "粗体 (Ctrl+B)", run: toggleBold },
    { label: "Italic", icon: <Italic className="h-4 w-4" />, title: "斜体 (Ctrl+I)", run: toggleItalic },
    {
      label: "Strikethrough",
      icon: <Strikethrough className="h-4 w-4" />,
      title: "删除线 (Ctrl+Shift+X)",
      run: toggleStrikethrough,
    },
    { label: "Code", icon: <Code className="h-4 w-4" />, title: "行内代码", run: toggleInlineCode },
    {
      label: "H1",
      icon: <Heading1 className="h-4 w-4" />,
      title: "标题 1",
      run: toggleHeading(1),
    },
    {
      label: "H2",
      icon: <Heading2 className="h-4 w-4" />,
      title: "标题 2",
      run: toggleHeading(2),
    },
    {
      label: "H3",
      icon: <Heading3 className="h-4 w-4" />,
      title: "标题 3",
      run: toggleHeading(3),
    },
    {
      label: "Bullet list",
      icon: <List className="h-4 w-4" />,
      title: "无序列表 (Ctrl+Shift+8)",
      run: toggleBulletList,
    },
    {
      label: "Ordered list",
      icon: <ListOrdered className="h-4 w-4" />,
      title: "有序列表 (Ctrl+Shift+7)",
      run: toggleOrderedList,
    },
    {
      label: "Task list",
      icon: <ListChecks className="h-4 w-4" />,
      title: "任务列表 (Ctrl+Shift+9)",
      run: toggleTaskList,
    },
    { label: "Table", icon: <Table className="h-4 w-4" />, title: "插入表格", run: insertTable },
    { label: "Link", icon: <Link className="h-4 w-4" />, title: "插入链接", run: insertLink },
  ];

  return (
    <div
      className={cn(
        "flex flex-wrap items-center gap-1 border-b bg-background p-1",
        className,
      )}
      role="toolbar"
      aria-label="编辑器工具栏"
    >
      {tools.map((tool) => (
        <Button
          key={tool.label}
          variant="ghost"
          size="icon"
          className="h-8 w-8"
          title={tool.title}
          aria-label={tool.label}
          onMouseDown={(e) => e.preventDefault()}
          onClick={() => {
            const view = getView();
            if (!view) return;
            view.focus();
            tool.run(view);
          }}
        >
          {tool.icon}
        </Button>
      ))}
    </div>
  );
}

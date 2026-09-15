/**
 * MarkdownBody —— Agent 面板助手消息（agentMessage）的 markdown 渲染封装
 *
 * 直接复用项目已有依赖 `streamdown`（基于 react-markdown + remark-gfm 的
 * 流式优化渲染器，SelectionToolbar / MessageStream 已在使用），不新增依赖：
 * - GFM 全套：标题/列表（含任务列表）/表格/代码块/行内码/链接/加粗/删除线等
 * - 流式：Streamdown 为增量追加设计（不完整 markdown 容错解析），流式更新时
 *   直接渲染即可；`streaming` 透传给 Streamdown 的 mode（streaming/static），
 *   完成后切 static 可跳过流式动画与重解析开销
 * - 安全：不碰 rehype 插件（默认管线 rehype-raw 后接 rehype-sanitize +
 *   rehype-harden，HTML 不会原样注入）；此处仅覆盖 `a` 强制
 *   target=_blank + rel=noreferrer（Tauri 内打开外链，避免劫持面板路由）
 */
import { lazy, memo, Suspense } from "react";
import type { Components } from "streamdown";

// streamdown 首屏不加载：AgentPanel 本就经 React.lazy 挂载，此处再包一层 lazy，
// streamdown（含 react-markdown/remark 系 + styles.css）进 AgentPanel 的 lazy chunk。
// 加载前降级为纯文本 pre（与 MessageStream 流式中占位同风格），避免布局跳变。
const LazyStreamdown = lazy(() =>
  import("streamdown").then((m) => ({ default: m.Streamdown })),
);

const agentComponents: Components = {
  a: ({ node, className, ...props }) => {
    void node;
    return (
      <a
        {...props}
        target="_blank"
        rel="noreferrer"
        className={["agent-md-link", className].filter(Boolean).join(" ")}
      />
    );
  },
};

type Props = {
  text: string;
  /** 流式追加中（对应 ItemView.complete === false） */
  streaming?: boolean;
};

export const MarkdownBody = memo(function MarkdownBody({ text, streaming }: Props) {
  return (
    <div className="agent-md">
      <Suspense
        fallback={
          <pre className="whitespace-pre-wrap break-words text-sm leading-relaxed">{text}</pre>
        }
      >
        <LazyStreamdown
          mode={streaming ? "streaming" : "static"}
          components={agentComponents}
        >
          {text}
        </LazyStreamdown>
      </Suspense>
    </div>
  );
});

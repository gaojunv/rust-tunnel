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
import { memo } from "react";
import { Streamdown } from "streamdown";
import type { Components } from "streamdown";

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
      <Streamdown mode={streaming ? "streaming" : "static"} components={agentComponents}>
        {text}
      </Streamdown>
    </div>
  );
});

import { isValidElement, cloneElement, useMemo } from "react";
import { Streamdown, CodeBlockCopyButton, type Components } from "streamdown";
import { cjk } from "@streamdown/cjk";
import "streamdown/styles.css";
import { transformWikilinks, parseWikilinkHref } from "@/lib/wikilink";
import { AttachmentImg } from "@/components/AttachmentImg";

// —— 代码块：单层带语言头的容器 + 官方复制按钮 ——
const PreFrame: Components["pre"] = ({ children }) => {
  if (!isValidElement(children)) return <>{children}</>;
  const codeEl = children as React.ReactElement<Record<string, unknown>>;
  const codeProps = (codeEl.props ?? {}) as Record<string, unknown> & {
    children?: React.ReactNode;
  };
  const language =
    /language-([\w-]+)/.exec((codeProps.className as string | undefined) ?? "")?.[1] ?? "";
  let raw = "";
  const inner = codeProps.children;
  if (isValidElement(inner) && typeof (inner.props as { children?: unknown }).children === "string") {
    raw = (inner.props as { children: string }).children;
  } else if (typeof inner === "string") {
    raw = inner;
  }
  return (
    <div className="my-3 overflow-hidden rounded-lg border border-border bg-muted/40">
      <div className="flex items-center justify-between border-b border-border/70 px-3 py-1.5">
        <span className="font-mono text-xs lowercase text-muted-foreground">{language || "text"}</span>
        <CodeBlockCopyButton code={raw} />
      </div>
      {cloneElement(codeEl, {
        "data-block": "true",
        className: codeProps.className,
      } as Record<string, unknown>)}
    </div>
  );
};

const PlainCode: Components["code"] = (props) => {
  const { children, className } = props;
  if (!("data-block" in props)) return <code className={className}>{children}</code>;
  return (
    <pre className={className}>
      <code>{children}</code>
    </pre>
  );
};

const Table: Components["table"] = ({ children }) => (
  <div className="my-3 overflow-x-auto rounded-lg border border-border">
    <table className="w-full border-collapse text-sm">{children}</table>
  </div>
);

const MD_CLASS = [
  "text-sm leading-7",
  "[&_h1]:!mt-4 [&_h1]:!mb-2 [&_h1]:!text-xl",
  "[&_h2]:!mt-4 [&_h2]:!mb-2 [&_h2]:!text-lg",
  "[&_h3]:!mt-3 [&_h3]:!mb-1.5 [&_h3]:!text-base",
  "[&_p]:!leading-7 [&_li]:!leading-7 [&_li]:!py-0.5 [&_ul]:!my-2 [&_ol]:!my-2",
  "[&_th]:!border-0 [&_th]:!border-r [&_th]:!border-border [&_th]:!bg-muted/60 [&_th]:!px-3 [&_th]:!py-1.5 [&_th]:!text-left [&_th]:!font-medium [&_th:last-child]:!border-r-0",
  "[&_td]:!border-0 [&_td]:!border-r [&_td]:!border-border [&_td]:!px-3 [&_td]:!py-1.5 [&_td:last-child]:!border-r-0",
  "[&_tr]:!border-0 [&_tr]:!border-t [&_tr]:!border-border [&_thead_tr]:!border-t-0",
  "[&_code:not(pre_code)]:!rounded [&_code:not(pre_code)]:!bg-muted [&_code:not(pre_code)]:!px-1 [&_code:not(pre_code)]:!py-0.5 [&_code:not(pre_code)]:!text-[0.875em] [&_code:not(pre_code)]:!font-mono",
  "[&_pre]:!bg-transparent dark:[&_pre]:!bg-transparent [&_pre]:!my-0 [&_pre]:!overflow-x-auto [&_pre]:!p-3 [&_pre]:!font-mono [&_pre]:!text-[13px] [&_pre]:!leading-relaxed",
].join(" ");

type Props = {
  content: string;
  onNavigate?: (key: string) => void;
};

function makeAnchor(onNavigate?: (key: string) => void): Components["a"] {
  const WikiA: Components["a"] = ({ children, href, ...rest }) => {
    const h = typeof href === "string" ? href : "";
    const target = parseWikilinkHref(h);
    if (target !== null) {
      return (
        <button
          type="button"
          onClick={() => onNavigate?.(target)}
          className="cursor-pointer rounded px-0.5 text-primary underline decoration-dotted underline-offset-4 hover:bg-accent/60"
        >
          {children}
        </button>
      );
    }
    return (
      <a
        href={h || undefined}
        target="_blank"
        rel="noreferrer"
        className="text-primary underline underline-offset-4"
        {...rest}
      >
        {children}
      </a>
    );
  };
  return WikiA;
}

export function MarkdownPreview({ content, onNavigate }: Props) {
  const transformed = useMemo(() => transformWikilinks(content), [content]);
  const anchor = useMemo(() => makeAnchor(onNavigate), [onNavigate]);

  return (
    <Streamdown
      className={MD_CLASS}
      plugins={{ cjk }}
      linkSafety={{ enabled: false }}
      components={{ a: anchor, pre: PreFrame, code: PlainCode, table: Table, img: AttachmentImg }}
    >
      {transformed}
    </Streamdown>
  );
}

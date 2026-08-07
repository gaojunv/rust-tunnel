import { memo, isValidElement, cloneElement } from 'react';
import { Streamdown, CodeBlockCopyButton, type Components } from 'streamdown';
import { code as codePlugin } from '@streamdown/code';
import { cjk } from '@streamdown/cjk';
import 'streamdown/styles.css';

/** pre 组件覆盖：Streamdown 默认 pre 只是 cloneElement 透传，code 插件管线会把
 *  Shiki 高亮 token 直接注入 <code> 的 children——所以透传的 code 元素里高亮
 *  是完整的。自己画单层框（对齐 Vercel AI Chatbot 的定制渲染器）：
 *  语言头（左语言名，右官方 CodeBlockCopyButton）+ 透传 code 高亮本体。
 *  不包官方 CodeBlock 容器——它的容器/头部/悬浮按钮三层结构正是要丢弃的部分。 */
const PreFrame: Components['pre'] = ({ children }) => {
  if (!isValidElement(children)) return <>{children}</>;
  const codeEl = children as React.ReactElement<Record<string, unknown>>;
  const codeProps = (codeEl.props ?? {}) as Record<string, unknown> & { children?: React.ReactNode };
  const language = /language-([\w-]+)/.exec((codeProps.className as string | undefined) ?? '')?.[1] ?? '';
  // code 的 children 是 <span>{raw}</span>（hast pre>code 结构），取原始代码串
  let raw = '';
  const inner = codeProps.children;
  if (isValidElement(inner) && typeof (inner.props as { children?: unknown }).children === 'string') {
    raw = (inner.props as { children: string }).children;
  } else if (typeof inner === 'string') {
    raw = inner;
  }
  return (
    <div className="my-3 overflow-hidden rounded-lg border border-border bg-muted/40">
      <div className="flex items-center justify-between border-b border-border/70 px-3 py-1.5">
        <span className="font-mono text-xs lowercase text-muted-foreground">{language || 'text'}</span>
        <CodeBlockCopyButton code={raw} />
      </div>
      {cloneElement(codeEl, {
        // data-block 是默认 pre 用来标记块级 code 的约定（code 组件靠它区分行内/块级），
        // 覆盖 pre 后必须保留，否则 code 退成行内渲染、丢失高亮；className 保留
        // 原值（含 language-xxx），code 组件靠它提取语言交给官方 CodeBlock
        'data-block': 'true',
        className: codeProps.className,
      } as Record<string, unknown>)}
    </div>
  );
};

/** 表格（components.table 覆盖）：横向滚动容器 + 干净网格。
 *  Streamdown 默认在表格外再套一层带复制按钮的 wrapper，结构臃肿，这里简化为单层。
 *  边框：容器 border 提供外框，单元格只画内部线（竖线 border-r 去末列，
 *  横线由 tr border-t 承担、表头行除外）——容器边框与任何单元格边框都不相邻，
 *  任何位置只有一条 1px 线，无双层。 */
const Table: Components['table'] = ({ children }) => (
  <div className="my-3 overflow-x-auto rounded-lg border border-border">
    <table className="w-full border-collapse text-sm">{children}</table>
  </div>
);

/** 助手消息 Markdown 渲染：Streamdown（流式半截语法容错 + Shiki 高亮），
 *  代码块/表格通过 components 覆盖为定制单层结构（见上）。
 *  高亮主题跟随页面明暗：light-plus / dark-plus（VS Code Light+/Dark+）。
 *  排版覆盖（容器 [&_...]:! 任意变体，特异性高于 Streamdown 内联类）：
 *  - 标题字号收敛到 xl/lg/base 梯度；正文/列表 leading-7 适配 CJK 长文
 *  - 表格单元格 th/td 横竖线 + 表头底色
 *  - pre 的 shiki 内联底色（--shiki-dark-bg 中性灰）置透明，由容器统一承载 */
const MD_CLASS = [
  'text-sm leading-7',
  '[&_h1]:!mt-4 [&_h1]:!mb-2 [&_h1]:!text-xl',
  '[&_h2]:!mt-4 [&_h2]:!mb-2 [&_h2]:!text-lg',
  '[&_h3]:!mt-3 [&_h3]:!mb-1.5 [&_h3]:!text-base',
  '[&_p]:!leading-7 [&_li]:!leading-7 [&_li]:!py-0.5 [&_ul]:!my-2 [&_ol]:!my-2',
  // 表格网格线：容器 div 的 border 不参与表格的 border-collapse 模型，所以
  // 任何贴着表格外缘的单元格边框都会与容器边框并列成双层 2px。规则：
  // - 竖线：th/td 只画 border-r，且末列去掉（否则与容器右边框双层）
  // - 横线：全部由 tr 的 border-t 承担（行顶线永远碰不到容器顶/底边框），
  //   表头行去顶线（[thead_tr] 精确选择；不能用 first: 变体——它作用于容器自身）
  // - 单元格不画 border-b（否则末行与容器底边框双层）
  '[&_th]:!border-0 [&_th]:!border-r [&_th]:!border-border [&_th]:!bg-muted/60 [&_th]:!px-3 [&_th]:!py-1.5 [&_th]:!text-left [&_th]:!font-medium [&_th:last-child]:!border-r-0',
  '[&_td]:!border-0 [&_td]:!border-r [&_td]:!border-border [&_td]:!px-3 [&_td]:!py-1.5 [&_td:last-child]:!border-r-0',
  '[&_tr]:!border-0 [&_tr]:!border-t [&_tr]:!border-border [&_thead_tr]:!border-t-0',
  // 行内代码
  '[&_code:not(pre_code)]:!rounded [&_code:not(pre_code)]:!bg-muted [&_code:not(pre_code)]:!px-1.5 [&_code:not(pre_code)]:!py-0.5 [&_code:not(pre_code)]:!text-[0.875em]',
  // 代码块：PreFrame 里官方 CodeBlock 容器嵌在我的框内——压掉官方三层结构实现
  // 单层视觉：容器去边距/边框/背景/padding、官方头部隐藏（语言名我的框已显示）、
  // 官方悬浮 actions 条隐藏（复制按钮我的框已提供）、body 去自身边框/背景只剩滚动区
  '[&_[data-streamdown=code-block]]:!m-0 [&_[data-streamdown=code-block]]:!gap-0 [&_[data-streamdown=code-block]]:!rounded-none [&_[data-streamdown=code-block]]:!border-0 [&_[data-streamdown=code-block]]:!bg-transparent [&_[data-streamdown=code-block]]:!p-0',
  '[&_[data-streamdown=code-block-header]]:!hidden',
  '[&_[data-streamdown=code-block-actions]]:!hidden',
  '[&_[data-streamdown=code-block]>.pointer-events-none]:!hidden',
  '[&_[data-streamdown=code-block-body]]:!rounded-none [&_[data-streamdown=code-block-body]]:!border-0 [&_[data-streamdown=code-block-body]]:!bg-transparent [&_[data-streamdown=code-block-body]]:!p-3',
  // shiki 内联底色（--shiki-dark-bg 中性灰）置透明，由我的容器 bg-muted/40 统一承载
  '[&_pre]:!bg-transparent dark:[&_pre]:!bg-transparent',
  '[&_pre_.block]:before:!text-[13px]',
].join(' ');

export default memo(function Markdown({ content }: { content: string }) {
  return (
    <Streamdown
      className={MD_CLASS}
      plugins={{ code: codePlugin, cjk }}
      shikiTheme={['light-plus', 'dark-plus']}
      controls={{ code: { copy: false, download: false } }}
      components={{ pre: PreFrame, table: Table }}
    >
      {content}
    </Streamdown>
  );
});

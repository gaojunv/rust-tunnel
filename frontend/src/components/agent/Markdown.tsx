import { memo } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import rehypeHighlight from 'rehype-highlight';

/** 助手消息 Markdown 渲染：GFM（表格/删除线/任务列表）+ 代码高亮。
 *  memo 化：流式期间列表整体重渲染时，内容未变的气泡跳过重解析。 */
export default memo(function Markdown({ content }: { content: string }) {
  return (
    <div className="markdown-body text-sm leading-relaxed [&_pre]:rounded-md [&_pre]:bg-muted [&_pre]:p-2 [&_code]:text-[0.85em] [&_table]:border-collapse [&_td]:border [&_td]:border-border [&_td]:px-2 [&_td]:py-1 [&_th]:border [&_th]:border-border [&_th]:px-2 [&_th]:py-1">
      <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeHighlight]}>
        {content}
      </ReactMarkdown>
    </div>
  );
});

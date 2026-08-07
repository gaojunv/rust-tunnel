import { memo } from 'react';
import { Streamdown } from 'streamdown';
import { code } from '@streamdown/code';
import { cjk } from '@streamdown/cjk';
import 'streamdown/styles.css';

/** 助手消息 Markdown 渲染：Streamdown（流式半截语法容错 + Shiki 高亮 + 表格/代码块控件）
 *  + @tailwindcss/typography 的 prose 排版节奏（标题/段落/列表/表格统一呼吸感）。
 *  memo 化：流式期间列表整体重渲染时，内容未变的气泡跳过重解析。
 *  controls：关闭下载/全屏等重交互控件，仅保留复制；表格下载在对话流里价值低。 */
export default memo(function Markdown({ content }: { content: string }) {
  return (
    <Streamdown
      className="prose prose-sm max-w-none dark:prose-invert prose-neutral"
      plugins={{ code, cjk }}
      shikiTheme={['github-light', 'github-dark']}
      controls={{ code: { copy: true, download: false }, table: { copy: true, download: false, fullscreen: false } }}
    >
      {content}
    </Streamdown>
  );
});

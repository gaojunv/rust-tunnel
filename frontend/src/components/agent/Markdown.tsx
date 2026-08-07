import { memo } from 'react';
import { Streamdown } from 'streamdown';
import { code } from '@streamdown/code';
import { cjk } from '@streamdown/cjk';
import 'streamdown/styles.css';

/** 助手消息 Markdown 渲染：Streamdown（流式半截语法容错 + Shiki 高亮 + 代码/表格控件）。
 *  memo 化：流式期间列表整体重渲染时，内容未变的气泡跳过重解析。
 *
 *  高亮主题跟随页面明暗：[亮色, 暗色] 双主题由 Streamdown/Shiki 按 html.dark 切换。
 *  light-plus / dark-plus 即 VS Code 默认的 Light+/Dark+（Shiki 内置主题中无
 *  "2026" 命名，VS Code 现行默认配色就是这两个）。
 *
 *  排版覆盖（容器 [&_...]:! 任意变体，特异性高于组件内联类）：
 *  - 标题字号收敛：默认 h1 text-3xl 在对话流里过于突兀，统一压到 xl/lg/base 梯度
 *  - 行高 1.75：CJK 长文阅读更舒适
 *  - 代码块：bg-sidebar 容器改为透明（去掉与内层 body 的"双层框"），仅保留内层
 *    border + bg-muted/40；行号列略放大提高可读性
 *  - 表格：默认只有横线（divide-y），补上竖向 divide-x 与单元格边框，恢复网格感
 *  - controls：仅保留复制，关闭下载/全屏等重交互控件 */
const MD_CLASS = [
  'text-sm leading-7',
  // 标题梯度：对话内不需要 3xl/2xl 的展示级字号
  '[&_h1]:!mt-4 [&_h1]:!mb-2 [&_h1]:!text-xl',
  '[&_h2]:!mt-4 [&_h2]:!mb-2 [&_h2]:!text-lg',
  '[&_h3]:!mt-3 [&_h3]:!mb-1.5 [&_h3]:!text-base',
  // 段落/列表行高与间距
  '[&_p]:!leading-7 [&_li]:!leading-7 [&_li]:!py-0.5 [&_ul]:!my-2 [&_ol]:!my-2',
  // 代码块：外层容器去背景去 padding（单层框），body 自带 border + 柔和底色
  '[&_[data-streamdown=code-block]]:!gap-0 [&_[data-streamdown=code-block]]:!border-0 [&_[data-streamdown=code-block]]:!bg-transparent [&_[data-streamdown=code-block]]:!p-0 [&_[data-streamdown=code-block]]:!pt-8',
  // pre 的 shiki 内联背景（--sdm-bg/--shiki-dark-bg，如 Dark+ 的 #1e1e1e 中性灰）
  // 与页面主题底色（暗色为深蓝）不一致 → 置透明，由 body 的 bg-muted/40 统一承载底色
  '[&_[data-streamdown=code-block-body]]:!bg-muted/40',
  '[&_[data-streamdown=code-block-body]_pre]:!bg-transparent dark:[&_[data-streamdown=code-block-body]_pre]:!bg-transparent',
  // 行号列：默认 13px 偏小，与正文字号对齐
  '[&_[data-streamdown=code-block-body]_.block]:before:!text-[13px]',
  // 表格：横线之外补竖线（divide-x + 单元格右边框），表头保持底色区分
  '[&_[data-streamdown=table-wrapper]]:!gap-0 [&_[data-streamdown=table-wrapper]]:!bg-transparent [&_[data-streamdown=table-wrapper]]:!p-0 [&_[data-streamdown=table-wrapper]]:!pt-6',
  '[&_[data-streamdown=table]]:!divide-x [&_[data-streamdown=table]]:!divide-border',
  '[&_[data-streamdown=table-header-cell]]:!border-r [&_[data-streamdown=table-header-cell]]:!border-border last:[&_[data-streamdown=table-header-cell]]:!border-r-0',
  '[&_[data-streamdown=table-cell]]:!border-r [&_[data-streamdown=table-cell]]:!border-border last:[&_[data-streamdown=table-cell]]:!border-r-0',
].join(' ');

export default memo(function Markdown({ content }: { content: string }) {
  return (
    <Streamdown
      className={MD_CLASS}
      plugins={{ code, cjk }}
      shikiTheme={['light-plus', 'dark-plus']}
      controls={{ code: { copy: true, download: false }, table: { copy: true, download: false, fullscreen: false } }}
    >
      {content}
    </Streamdown>
  );
});

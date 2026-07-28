import { cn } from '../../lib/utils';

export interface GridWaveTitleProps {
  text: string;
  className?: string;
  /**
   * @deprecated 网格画布现在由 PageHeader 直接渲染（覆盖整个卡片），
   * GridWaveTitle 只负责渲染标题文字本身。保留此 prop 仅为兼容签名。
   */
  eventTargetRef?: React.RefObject<HTMLElement | null>;
}

/**
 * Grid Wave 模式下的标题文字。画布已上移到 PageHeader（覆盖整个卡片），
 * 这里只渲染带极光渐变流光的标题文字。
 */
export function GridWaveTitle({ text, className }: GridWaveTitleProps) {
  return <span className={cn('text-aurora', className)}>{text}</span>;
}

import { AlertCircle, AlertTriangle, CircleStop, Info } from 'lucide-react';
import { Separator } from '@/components/ui/separator';

export type SystemTone = 'info' | 'warning' | 'error' | 'stopped';

interface Props {
  content: string;
  tone?: SystemTone;
}

/** tone → 图标（warning/error 专属配色，info/stopped 走 muted）。 */
const ICON_CLS: Record<SystemTone, string> = {
  info: 'text-muted-foreground',
  warning: 'text-amber-500',
  error: 'text-destructive',
  stopped: 'text-muted-foreground',
};

/** tone → 文本配色（error/warning 专属，其余 muted）。 */
const TEXT_CLS: Record<SystemTone, string> = {
  info: 'text-muted-foreground',
  warning: 'text-amber-500',
  error: 'text-destructive',
  stopped: 'text-muted-foreground',
};

const TONE_ICON: Record<SystemTone, typeof Info> = {
  info: Info,
  warning: AlertTriangle,
  error: AlertCircle,
  stopped: CircleStop,
};

/** 独立系统提示行：状态/错误/中断等过程性提示，两侧分隔线隔离出提示区。 */
export default function SystemMessage({ content, tone = 'info' }: Props) {
  const Icon = TONE_ICON[tone];
  return (
    <div className="my-1.5 flex items-center gap-2" role="status">
      <Separator className="flex-1" />
      <Icon className={`h-3 w-3 shrink-0 ${ICON_CLS[tone]}`} />
      <span className={`text-xs ${TEXT_CLS[tone]}`}>{content}</span>
      <Separator className="flex-1" />
    </div>
  );
}

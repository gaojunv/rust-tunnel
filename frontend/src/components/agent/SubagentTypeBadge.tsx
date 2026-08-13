import { useTranslation } from 'react-i18next';
import { subagentTypeMeta } from './subagent';

/** subagent 类型徽标：已知类型本地化显示名 + 语义色 chip（与工具卡 KindChip 的
 *  「淡色底 + 彩色文字」语言一致），未知类型回退原值 + muted 灰。固定面板行与
 *  SubagentTaskCard 头部两处共用，保证类型显示一致。 */
export default function SubagentTypeBadge({ type }: { type: string }) {
  const { t } = useTranslation();
  const meta = subagentTypeMeta(type);
  if (!meta) return null;
  return (
    <span className={`shrink-0 rounded px-1.5 py-0.5 text-[10px] ${meta.chipClass} ${meta.textClass}`}>
      {meta.labelKey ? t(meta.labelKey) : type}
    </span>
  );
}

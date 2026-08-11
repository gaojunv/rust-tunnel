import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { ShieldAlert, Check, CheckCheck, X, Ban } from 'lucide-react';
import type { ChatItem } from './types';

interface Props {
  item: ChatItem;
  onRespond: (id: string, approved: boolean, remember: boolean, optionId?: string) => void;
}

export default function ApprovalCard({ item, onRespond }: Props) {
  const { t } = useTranslation();
  const status = item.approvalStatus ?? 'pending';
  const pending = status === 'pending';
  const options = item.approvalOptions;
  // 三态视觉区分：pending 保持琥珀高亮；approved 走 emerald；denied 走
  // destructive；expired 被动过期 → 灰色 + 虚线边框（区别于用户主动拒绝）
  const CARD_CLS: Record<string, string> = {
    pending: 'border-amber-500/50 bg-amber-500/10',
    approved: 'border-emerald-500/50 bg-emerald-500/10',
    denied: 'border-destructive/50 bg-destructive/10',
    expired: 'border-dashed border-border bg-muted/30 opacity-70',
  };
  const ICON_CLS: Record<string, string> = {
    pending: 'text-amber-500',
    approved: 'text-emerald-600',
    denied: 'text-destructive',
    expired: 'text-muted-foreground',
  };
  // 终态徽章（替换原先底部状态文案）：复用既有 i18n key，不硬编码
  const BADGE_CLS: Record<string, string> = {
    approved: 'border-emerald-500/40 bg-emerald-500/10 text-emerald-600',
    denied: 'border-destructive/40 bg-destructive/10 text-destructive',
    expired: 'border-border bg-muted text-muted-foreground',
  };
  const badgeText =
    status === 'approved' ? `✓ ${t('agent.approved')}`
    : status === 'denied' ? `✗ ${t('agent.denied')}`
    : t('agent.approvalExpired');
  // ACP 权限选项语义映射：allow_* → 放行（allow_always 附带本会话记住），
  // reject_* → 拒绝；自定义 kind 走中性样式。allow 类点选后卡片显 ✓。
  const isAllow = (kind: string | undefined) => kind?.startsWith('allow') ?? false;
  const isRemember = (kind: string | undefined) => kind === 'allow_always';

  const optionButtonClass = (kind: string | undefined) => {
    if (kind?.startsWith('allow')) return 'border-green-500/50 text-green-600 hover:bg-green-500/10';
    if (kind?.startsWith('reject')) return 'border-destructive/60 text-destructive hover:bg-destructive/10';
    return 'border-border text-foreground hover:bg-muted';
  };
  const optionIcon = (kind: string | undefined) => {
    if (kind === 'allow_always') return <CheckCheck className="mr-1 h-3.5 w-3.5" />;
    if (kind?.startsWith('allow')) return <Check className="mr-1 h-3.5 w-3.5" />;
    if (kind === 'reject_always') return <Ban className="mr-1 h-3.5 w-3.5" />;
    return <X className="mr-1 h-3.5 w-3.5" />;
  };

  return (
    <div className={`rounded-lg border p-3 text-sm ${CARD_CLS[status]}`}>
      <div className="mb-1.5 flex items-center gap-1.5 font-medium">
        <ShieldAlert className={`h-4 w-4 ${ICON_CLS[status]}`} />
        <span className="min-w-0 truncate">
          {t('agent.approvalRequired')}: <code>{item.approvalTool}</code>
        </span>
        {!pending && (
          <span className={`ml-auto inline-flex items-center rounded-full border px-2 py-0.5 text-[11px] font-medium ${BADGE_CLS[status]}`}>
            {badgeText}
          </span>
        )}
      </div>
      {/* 命令摘要单行截断：长命令不换行撑开卡片；悬停 title 查看全文 */}
      <pre
        className="mb-2 truncate rounded bg-background/60 px-2 py-1.5 text-xs"
        title={item.approvalSummary}
      >
        {item.approvalSummary}
      </pre>
      {pending && (
        options && options.length > 0 ? (
          // ACP 选项透传：每个选项一个按钮，用户点选回传 option_id
          <div className="flex flex-wrap gap-2">
            {options.map((opt) => (
              <Button
                key={opt.id}
                size="sm"
                variant="outline"
                className={optionButtonClass(opt.kind)}
                onClick={() => item.approvalId && onRespond(item.approvalId, isAllow(opt.kind), isRemember(opt.kind), opt.id)}
              >
                {optionIcon(opt.kind)}{opt.label}
              </Button>
            ))}
          </div>
        ) : (
          <div className="flex gap-2">
            <Button size="sm" variant="outline" onClick={() => item.approvalId && onRespond(item.approvalId, true, false)}>
              <Check className="mr-1 h-3.5 w-3.5" />{t('agent.approveOnce')}
            </Button>
            <Button size="sm" variant="outline" onClick={() => item.approvalId && onRespond(item.approvalId, true, true)}>
              {t('agent.approveSession')}
            </Button>
            <Button size="sm" variant="destructive" onClick={() => item.approvalId && onRespond(item.approvalId, false, false)}>
              <X className="mr-1 h-3.5 w-3.5" />{t('agent.deny')}
            </Button>
          </div>
        )
      )}
    </div>
  );
}

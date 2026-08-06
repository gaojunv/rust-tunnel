import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { ShieldAlert, Check, X } from 'lucide-react';
import type { ChatItem } from './types';

interface Props {
  item: ChatItem;
  onRespond: (id: string, approved: boolean, remember: boolean) => void;
}

export default function ApprovalCard({ item, onRespond }: Props) {
  const { t } = useTranslation();
  const pending = item.approvalStatus === 'pending';
  return (
    <div className={`rounded-lg border p-3 text-sm ${pending ? 'border-amber-500/50 bg-amber-500/10' : 'border-border bg-muted/30 opacity-70'}`}>
      <div className="mb-1.5 flex items-center gap-1.5 font-medium">
        <ShieldAlert className="h-4 w-4 text-amber-500" />
        {t('agent.approvalRequired')}: <code>{item.approvalTool}</code>
      </div>
      <pre className="mb-2 overflow-x-auto whitespace-pre-wrap break-all rounded bg-background/60 px-2 py-1.5 text-xs">
        {item.approvalSummary}
      </pre>
      {pending ? (
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
      ) : (
        <p className="text-xs text-muted-foreground">
          {item.approvalStatus === 'approved' ? `✓ ${t('agent.approved')}` : `✗ ${t('agent.denied')}`}
        </p>
      )}
    </div>
  );
}

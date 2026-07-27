import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/badge';
import { cn } from '@/lib/utils';
import type { CertificateStatus } from '@/types';

const statusConfig: Record<CertificateStatus, { tone: string; dot: string }> = {
  pending: {
    tone: 'bg-amber-500/10 text-amber-500 border-amber-500/25',
    dot: 'bg-amber-500 shadow-[0_0_6px_hsl(38_92%_55%/0.8)]',
  },
  active: {
    tone: 'bg-emerald-500/10 text-emerald-500 border-emerald-500/25',
    dot: 'bg-emerald-500 shadow-[0_0_6px_hsl(160_84%_45%/0.8)]',
  },
  expired: {
    tone: 'bg-red-500/10 text-red-500 border-red-500/25',
    dot: 'bg-red-500 shadow-[0_0_6px_hsl(0_72%_51%/0.8)]',
  },
  failed: {
    tone: 'bg-red-500/10 text-red-500 border-red-500/25',
    dot: 'bg-red-500 shadow-[0_0_6px_hsl(0_72%_51%/0.8)]',
  },
};

interface AcmeStatusBadgeProps {
  status: CertificateStatus;
  className?: string;
}

export function AcmeStatusBadge({ status, className }: AcmeStatusBadgeProps) {
  const { t } = useTranslation();
  const config = statusConfig[status] ?? statusConfig.pending;
  return (
    <Badge variant="outline" className={cn('gap-1.5 font-medium', config.tone, className)}>
      <span className={cn('h-1.5 w-1.5 rounded-full', config.dot)} />
      {t(`acme.status.${status}`)}
    </Badge>
  );
}

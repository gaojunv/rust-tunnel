import { Badge } from '@/components/ui/badge';
import type { CertificateStatus } from '@/types';

const statusConfig: Record<CertificateStatus, { label: string; className: string }> = {
  pending: { label: 'Pending', className: 'bg-yellow-500/10 text-yellow-500' },
  active: { label: 'Active', className: 'bg-green-500/10 text-green-500' },
  expired: { label: 'Expired', className: 'bg-red-500/10 text-red-500' },
  failed: { label: 'Failed', className: 'bg-red-500/10 text-red-500' },
};

interface AcmeStatusBadgeProps {
  status: CertificateStatus;
}

export function AcmeStatusBadge({ status }: AcmeStatusBadgeProps) {
  const config = statusConfig[status] ?? statusConfig.pending;
  return (
    <Badge variant="secondary" className={config.className}>
      {config.label}
    </Badge>
  );
}

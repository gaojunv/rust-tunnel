import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import { AlertCircle, CheckCircle, Clock } from 'lucide-react';
import { useChallengeStatus } from '@/api/hooks';

interface ChallengeStatusCardProps {
  domain: string;
}

function getStatusIcon(status: string) {
  switch (status) {
    case 'verified':
      return <CheckCircle className="h-4 w-4 text-emerald-500" />;
    case 'failed':
      return <AlertCircle className="h-4 w-4 text-red-500" />;
    case 'pending':
    default:
      return <Clock className="h-4 w-4 text-amber-500" />;
  }
}

function getStatusBadge(status: string) {
  switch (status) {
    case 'verified':
      return (
        <Badge
          variant="outline"
          className="gap-1.5 font-medium bg-emerald-500/10 text-emerald-500 border-emerald-500/25"
        >
          <span className="h-1.5 w-1.5 rounded-full bg-emerald-500 shadow-[0_0_6px_hsl(160_84%_45%/0.8)]" />
          Verified
        </Badge>
      );
    case 'failed':
      return (
        <Badge
          variant="outline"
          className="gap-1.5 font-medium bg-red-500/10 text-red-500 border-red-500/25"
        >
          <span className="h-1.5 w-1.5 rounded-full bg-red-500 shadow-[0_0_6px_hsl(0_72%_51%/0.8)]" />
          Failed
        </Badge>
      );
    case 'pending':
    default:
      return (
        <Badge
          variant="outline"
          className="gap-1.5 font-medium bg-amber-500/10 text-amber-500 border-amber-500/25"
        >
          <span className="h-1.5 w-1.5 rounded-full bg-amber-500 shadow-[0_0_6px_hsl(38_92%_55%/0.8)]" />
          Pending
        </Badge>
      );
  }
}

function getChallengeTypeLabel(type: string) {
  switch (type) {
    case 'dns-01':
      return 'DNS-01';
    case 'http-01':
    default:
      return 'HTTP-01';
  }
}

export function ChallengeStatusCard({ domain }: ChallengeStatusCardProps) {
  const { data: status, isLoading } = useChallengeStatus(domain);

  if (isLoading) {
    return (
      <Card>
        <CardContent className="py-8 text-center text-muted-foreground">
          Loading challenge status...
        </CardContent>
      </Card>
    );
  }

  if (!status) return null;

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between">
        <CardTitle className="flex items-center gap-2 text-base">
          {getStatusIcon(status.status)}
          Challenge Status
        </CardTitle>
        {getStatusBadge(status.status)}
      </CardHeader>
      <CardContent>
        <div className="grid gap-4 md:grid-cols-3 text-sm">
          <div>
            <div className="text-muted-foreground">Domain</div>
            <div className="font-mono">{status.domain}</div>
          </div>
          <div>
            <div className="text-muted-foreground">Challenge Type</div>
            <div>{getChallengeTypeLabel(status.type)}</div>
          </div>
          <div>
            <div className="text-muted-foreground">Status</div>
            <div className="flex items-center gap-2">
              {getStatusIcon(status.status)}
              {status.status.charAt(0).toUpperCase() + status.status.slice(1)}
            </div>
          </div>
        </div>
        {status.error && (
          <div className="mt-4 rounded-lg border border-destructive/25 bg-destructive/10 p-3 text-sm text-destructive">
            {status.error}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

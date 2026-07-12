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
      return <CheckCircle className="h-4 w-4 text-green-500" />;
    case 'failed':
      return <AlertCircle className="h-4 w-4 text-red-500" />;
    case 'pending':
    default:
      return <Clock className="h-4 w-4 text-yellow-500" />;
  }
}

function getStatusBadge(status: string) {
  switch (status) {
    case 'verified':
      return <Badge className="bg-green-500/10 text-green-700 border-green-200">Verified</Badge>;
    case 'failed':
      return <Badge variant="destructive">Failed</Badge>;
    case 'pending':
    default:
      return <Badge variant="secondary">Pending</Badge>;
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
          <div className="mt-4 rounded-md bg-destructive/10 p-3 text-sm text-destructive">
            {status.error}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

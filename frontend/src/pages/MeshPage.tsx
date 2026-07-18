import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { PageHeader } from '@/components/layout/PageHeader';
import { useClients } from '@/api/hooks';
import { cn } from '@/lib/utils';

type StatusTone = 'critical' | 'warning' | 'connected' | 'unknown';

const STATUS_TONES: Record<StatusTone, string> = {
  critical: 'bg-red-500/10 text-red-500 border-red-500/25',
  warning: 'bg-amber-500/10 text-amber-500 border-amber-500/25',
  connected: 'bg-emerald-500/10 text-emerald-500 border-emerald-500/25',
  unknown: 'text-muted-foreground',
};

const STATUS_DOTS: Record<StatusTone, string> = {
  critical: 'bg-red-500 shadow-[0_0_6px_hsl(0_72%_51%/0.8)]',
  warning: 'bg-amber-500 shadow-[0_0_6px_hsl(38_92%_55%/0.8)]',
  connected: 'bg-emerald-500 shadow-[0_0_6px_hsl(160_84%_45%/0.8)]',
  unknown: 'bg-muted-foreground/50',
};

const STATUS_LABELS: Record<StatusTone, string> = {
  critical: 'Critical',
  warning: 'Warning',
  connected: 'Connected',
  unknown: 'Unknown',
};

export default function MeshPage() {
  const { data: clients, isLoading } = useClients();

  return (
    <div className="space-y-6">
      <PageHeader
        title="Mesh Network"
        description="View mesh network connections and members"
      />

      <Card>
        <CardHeader>
          <CardTitle>Clients</CardTitle>
        </CardHeader>
        <CardContent>
          {isLoading ? (
            <div className="text-center py-8 text-muted-foreground">
              Loading...
            </div>
          ) : clients?.length === 0 ? (
            <div className="text-center py-8 text-muted-foreground">
              No clients connected
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Port</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead>Connections</TableHead>
                  <TableHead>Services</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {clients?.map((client) => {
                  const tone: StatusTone = client.quality?.is_critical
                    ? 'critical'
                    : client.quality?.is_warning
                      ? 'warning'
                      : client.quality
                        ? 'connected'
                        : 'unknown';
                  return (
                    <TableRow key={client.port}>
                      <TableCell className="font-medium">
                        {client.port}
                      </TableCell>
                      <TableCell>
                        <Badge
                          variant="outline"
                          className={cn('gap-1.5 font-medium', STATUS_TONES[tone])}
                        >
                          <span
                            className={cn(
                              'h-1.5 w-1.5 rounded-full',
                              STATUS_DOTS[tone]
                            )}
                          />
                          {STATUS_LABELS[tone]}
                        </Badge>
                      </TableCell>
                      <TableCell>{client.connection_count}</TableCell>
                      <TableCell className="text-muted-foreground">
                        {client.hostname ?? '-'}
                      </TableCell>
                    </TableRow>
                  );
                })}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  );
}

import { useNavigate } from 'react-router-dom';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { Button } from '@/components/ui/button';
import { StatCard } from '@/components/shared/StatCard';
import { QualityBadge } from '@/components/shared/QualityBadge';
import { PageHeader } from '@/components/layout/PageHeader';
import { useClients, useMetrics } from '@/api/hooks';
import {
  Users,
  Activity,
  ArrowDown,
  ArrowUp,
  ExternalLink,
} from 'lucide-react';

function formatBytes(bytes: number): string {
  if (bytes === 0) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
}

export default function DashboardPage() {
  const navigate = useNavigate();
  const { data: clients, isLoading: clientsLoading } = useClients();
  const { data: metrics, isLoading: metricsLoading } = useMetrics();

  const connectedClients = metrics?.client_count ?? 0;
  const activeConnections = metrics?.active_connection_count ?? 0;
  const totalBytesIn = metrics?.total_bytes_in ?? 0;
  const totalBytesOut = metrics?.total_bytes_out ?? 0;

  return (
    <div className="space-y-6">
      <PageHeader
        title="Dashboard"
        description="Overview of your tunnel connections"
      />

      {/* Stats Grid */}
      <div className="grid gap-4 md:grid-cols-2 md:gap-6 lg:grid-cols-4">
        <StatCard
          title="Connected Clients"
          value={connectedClients}
          icon={<Users className="h-4 w-4" />}
        />
        <StatCard
          title="Active Connections"
          value={activeConnections}
          icon={<Activity className="h-4 w-4" />}
        />
        <StatCard
          title="Total Bytes In"
          value={formatBytes(totalBytesIn)}
          icon={<ArrowDown className="h-4 w-4" />}
        />
        <StatCard
          title="Total Bytes Out"
          value={formatBytes(totalBytesOut)}
          icon={<ArrowUp className="h-4 w-4" />}
        />
      </div>

      {/* Client List */}
      <Card>
        <CardHeader>
          <CardTitle>Clients</CardTitle>
        </CardHeader>
        <CardContent>
          {clientsLoading || metricsLoading ? (
            <div className="py-12 text-center text-sm text-muted-foreground">
              Loading...
            </div>
          ) : clients?.length === 0 ? (
            <div className="flex flex-col items-center justify-center gap-2 py-12 text-muted-foreground">
              <Users className="h-8 w-8 opacity-40" />
              <p className="text-sm">No clients connected</p>
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead className="text-xs uppercase tracking-wider">
                    Port
                  </TableHead>
                  <TableHead className="text-xs uppercase tracking-wider">
                    Quality
                  </TableHead>
                  <TableHead className="text-xs uppercase tracking-wider">
                    RTT
                  </TableHead>
                  <TableHead className="text-xs uppercase tracking-wider">
                    Loss
                  </TableHead>
                  <TableHead className="text-xs uppercase tracking-wider">
                    Connections
                  </TableHead>
                  <TableHead className="text-right text-xs uppercase tracking-wider">
                    Actions
                  </TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {clients?.map((client) => (
                  <TableRow key={client.port}>
                    <TableCell className="font-medium tabular-nums">
                      {client.port}
                    </TableCell>
                    <TableCell>
                      <QualityBadge
                        score={client.quality?.quality_score ?? 0}
                      />
                    </TableCell>
                    <TableCell className="tabular-nums text-muted-foreground">
                      {client.quality?.last_rtt_ms != null
                        ? `${client.quality.last_rtt_ms.toFixed(1)}ms`
                        : '-'}
                    </TableCell>
                    <TableCell className="tabular-nums text-muted-foreground">
                      {client.quality?.loss_rate != null
                        ? `${(client.quality.loss_rate * 100).toFixed(1)}%`
                        : '-'}
                    </TableCell>
                    <TableCell className="tabular-nums text-muted-foreground">
                      {client.connection_count}
                    </TableCell>
                    <TableCell className="text-right">
                      <Button
                        variant="ghost"
                        size="icon"
                        className="h-8 w-8 text-muted-foreground hover:text-foreground"
                        onClick={() => navigate(`/clients/${client.port}`)}
                      >
                        <ExternalLink className="h-4 w-4" />
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  );
}

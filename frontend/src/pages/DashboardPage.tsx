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
      <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-4">
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
                  <TableHead>Quality</TableHead>
                  <TableHead>RTT</TableHead>
                  <TableHead>Loss</TableHead>
                  <TableHead>Connections</TableHead>
                  <TableHead>Actions</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {clients?.map((client) => (
                  <TableRow key={client.port}>
                    <TableCell className="font-medium">{client.port}</TableCell>
                    <TableCell>
                      <QualityBadge
                        score={client.quality?.quality_score ?? 0}
                      />
                    </TableCell>
                    <TableCell>
                      {client.quality?.last_rtt_ms != null
                        ? `${client.quality.last_rtt_ms.toFixed(1)}ms`
                        : '-'}
                    </TableCell>
                    <TableCell>
                      {client.quality?.loss_rate != null
                        ? `${(client.quality.loss_rate * 100).toFixed(1)}%`
                        : '-'}
                    </TableCell>
                    <TableCell>{client.connection_count}</TableCell>
                    <TableCell>
                      <Button
                        variant="ghost"
                        size="sm"
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

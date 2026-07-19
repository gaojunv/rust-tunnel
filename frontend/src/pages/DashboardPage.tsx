import { useNavigate } from 'react-router-dom';
import { useQuery } from '@tanstack/react-query';
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
import { PageHeader } from '@/components/layout/PageHeader';
import { clientsApi } from '@/api/client';
import { useMetrics } from '@/api/hooks';
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
  const { data: clients = [], isLoading: clientsLoading } = useQuery({
    queryKey: ['clients', 'dashboard'],
    queryFn: () => clientsApi.list(),
    refetchInterval: 5000,
  });
  const { data: metrics, isLoading: metricsLoading } = useMetrics();

  const connectedClients = clients.filter((c) => c.online).length;
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
          ) : clients.length === 0 ? (
            <div className="flex flex-col items-center justify-center gap-2 py-12 text-muted-foreground">
              <Users className="h-8 w-8 opacity-40" />
              <p className="text-sm">No clients registered</p>
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead className="text-xs uppercase tracking-wider">
                    Name
                  </TableHead>
                  <TableHead className="text-xs uppercase tracking-wider">
                    Hostname
                  </TableHead>
                  <TableHead className="text-xs uppercase tracking-wider">
                    Status
                  </TableHead>
                  <TableHead className="text-xs uppercase tracking-wider">
                    Version
                  </TableHead>
                  <TableHead className="text-xs uppercase tracking-wider">
                    Referenced
                  </TableHead>
                  <TableHead className="text-right text-xs uppercase tracking-wider">
                    Actions
                  </TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {clients.map((client) => (
                  <TableRow key={client.name}>
                    <TableCell className="font-medium">{client.name}</TableCell>
                    <TableCell className="text-muted-foreground">
                      {client.hostname ?? '-'}
                    </TableCell>
                    <TableCell>
                      <span
                        className={`inline-flex items-center rounded-full px-2 py-1 text-xs font-medium ${
                          client.online
                            ? 'bg-emerald-500/10 text-emerald-500'
                            : 'bg-muted text-muted-foreground'
                        }`}
                      >
                        {client.online ? 'online' : 'offline'}
                      </span>
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {client.client_version ?? '-'}
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {client.referenced_by_rules}
                    </TableCell>
                    <TableCell className="text-right">
                      <Button
                        variant="ghost"
                        size="icon"
                        className="h-8 w-8 text-muted-foreground hover:text-foreground"
                        onClick={() => navigate(`/clients/${client.name}`)}
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

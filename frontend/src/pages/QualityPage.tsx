import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { StatCard } from '@/components/shared/StatCard';
import { QualityBadge } from '@/components/shared/QualityBadge';
import { PageHeader } from '@/components/layout/PageHeader';
import { useQualitySummary } from '@/api/hooks';
import { Signal, AlertTriangle, Activity } from 'lucide-react';
import { cn } from '@/lib/utils';

export default function QualityPage() {
  const { data: summary, isLoading } = useQualitySummary();

  const totalConnections = summary?.total_connections ?? 0;
  const warningCount = summary?.warning_count ?? 0;
  const averageScore = summary?.average_score ?? 0;

  const getHeatmapColor = (score: number) => {
    if (score >= 80) return 'bg-green-500/20 border-green-500/30';
    if (score >= 60) return 'bg-yellow-500/20 border-yellow-500/30';
    if (score >= 40) return 'bg-orange-500/20 border-orange-500/30';
    return 'bg-red-500/20 border-red-500/30';
  };

  return (
    <div className="space-y-6">
      <PageHeader
        title="Connection Quality"
        description="Monitor connection quality across all clients"
      />

      {/* Stats */}
      <div className="grid gap-4 md:grid-cols-3">
        <StatCard
          title="Total Connections"
          value={totalConnections}
          icon={<Activity className="h-4 w-4" />}
        />
        <StatCard
          title="Warnings"
          value={warningCount}
          icon={<AlertTriangle className="h-4 w-4" />}
          trend={warningCount > 0 ? 'down' : 'neutral'}
        />
        <StatCard
          title="Average Quality"
          value={averageScore.toFixed(1)}
          icon={<Signal className="h-4 w-4" />}
        />
      </div>

      {/* Quality Heatmap */}
      <Card>
        <CardHeader>
          <CardTitle>Quality Heatmap</CardTitle>
        </CardHeader>
        <CardContent>
          {isLoading ? (
            <div className="text-center py-8 text-muted-foreground">
              Loading...
            </div>
          ) : !summary?.clients?.length ? (
            <div className="text-center py-8 text-muted-foreground">
              No clients
            </div>
          ) : (
            <div className="grid grid-cols-2 gap-2 sm:grid-cols-3 md:grid-cols-4 lg:grid-cols-6">
              {summary.clients.map(
                (client: {
                  port: number;
                  hostname?: string;
                  score: number;
                }) => (
                  <Card
                    key={client.port}
                    className={cn(
                      'text-center',
                      getHeatmapColor(client.score)
                    )}
                  >
                    <CardContent className="p-3">
                      <div className="text-sm font-medium">
                        {client.hostname
                          ? `${client.hostname}:${client.port}`
                          : `Port ${client.port}`}
                      </div>
                      <div className="text-2xl font-bold">{client.score}</div>
                    </CardContent>
                  </Card>
                )
              )}
            </div>
          )}
        </CardContent>
      </Card>

      {/* Worst Connections */}
      <Card>
        <CardHeader>
          <CardTitle>Worst Connections</CardTitle>
        </CardHeader>
        <CardContent>
          {isLoading ? (
            <div className="text-center py-8 text-muted-foreground">
              Loading...
            </div>
          ) : !summary?.worst?.length ? (
            <div className="text-center py-8 text-muted-foreground">
              No data
            </div>
          ) : (
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Port</TableHead>
                  <TableHead>Quality</TableHead>
                  <TableHead>RTT</TableHead>
                  <TableHead>Loss</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {summary.worst.map(
                  (client: {
                    port: number;
                    score: number;
                    rtt: number;
                    loss: number;
                  }) => (
                    <TableRow key={client.port}>
                      <TableCell className="font-medium">
                        {client.port}
                      </TableCell>
                      <TableCell>
                        <QualityBadge score={client.score} />
                      </TableCell>
                      <TableCell>
                        {client.rtt > 0 ? `${client.rtt.toFixed(1)}ms` : '-'}
                      </TableCell>
                      <TableCell>
                        {client.loss > 0
                          ? `${client.loss.toFixed(1)}%`
                          : '-'}
                      </TableCell>
                    </TableRow>
                  )
                )}
              </TableBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  );
}

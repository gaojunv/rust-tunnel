import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table';
import { QualityScoreSparkline } from '@/components/charts/QualityScoreSparkline';
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

  const getHeatmapTone = (score: number) => {
    if (score >= 80)
      return 'border-emerald-500/25 bg-emerald-500/10 text-emerald-500';
    if (score >= 60) return 'border-sky-500/25 bg-sky-500/10 text-sky-500';
    if (score >= 40)
      return 'border-amber-500/25 bg-amber-500/10 text-amber-500';
    return 'border-red-500/25 bg-red-500/10 text-red-500';
  };

  return (
    <div className="space-y-6">
      <PageHeader
        title="Connection Quality"
        description="Monitor connection quality across all clients"
      />

      {/* Stats */}
      <div className="grid gap-4 md:grid-cols-3 md:gap-6">
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
            <div className="py-12 text-center text-sm text-muted-foreground">
              Loading...
            </div>
          ) : !summary?.clients?.length ? (
            <div className="flex flex-col items-center justify-center gap-2 py-12 text-muted-foreground">
              <Signal className="h-8 w-8 opacity-40" />
              <p className="text-sm">No clients</p>
            </div>
          ) : (
            <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 md:grid-cols-4 lg:grid-cols-6">
              {summary.clients.map(
                (client: {
                  port: number;
                  hostname?: string;
                  score: number;
                }) => (
                  <div
                    key={client.port}
                    className={cn(
                      'rounded-lg border p-3 text-center transition-all duration-300 hover:shadow-md',
                      getHeatmapTone(client.score)
                    )}
                  >
                    <div className="truncate text-xs font-medium">
                      {client.hostname
                        ? `${client.hostname}:${client.port}`
                        : `Port ${client.port}`}
                    </div>
                    <div className="mt-1 text-2xl font-bold tabular-nums">
                      {client.score}
                    </div>
                    <QualityScoreSparkline port={client.port} />
                  </div>
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
            <div className="py-12 text-center text-sm text-muted-foreground">
              Loading...
            </div>
          ) : !summary?.worst?.length ? (
            <div className="py-12 text-center text-sm text-muted-foreground">
              No data
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
                      <TableCell className="font-medium tabular-nums">
                        {client.port}
                      </TableCell>
                      <TableCell>
                        <QualityBadge score={client.score} />
                      </TableCell>
                      <TableCell className="tabular-nums text-muted-foreground">
                        {client.rtt > 0 ? `${client.rtt.toFixed(1)}ms` : '-'}
                      </TableCell>
                      <TableCell className="tabular-nums text-muted-foreground">
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

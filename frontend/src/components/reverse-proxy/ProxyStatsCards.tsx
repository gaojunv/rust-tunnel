import { StatCard } from '@/components/shared/StatCard';
import { Network, Activity, Link, ArrowDownUp } from 'lucide-react';
import { useProxyStats } from '@/api/hooks';
import { formatBytes } from '@/utils/format';

export function ProxyStatsCards() {
  const { data: stats, isLoading } = useProxyStats();

  return (
    <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-4">
      <StatCard
        title="Total Rules"
        value={isLoading ? '—' : (stats?.total_rules ?? 0)}
        icon={<Network className="h-4 w-4" />}
      />
      <StatCard
        title="Active Rules"
        value={isLoading ? '—' : (stats?.active_rules ?? 0)}
        icon={<Activity className="h-4 w-4" />}
      />
      <StatCard
        title="Total Connections"
        value={isLoading ? '—' : (stats?.total_connections ?? 0)}
        icon={<Link className="h-4 w-4" />}
      />
      <StatCard
        title="Total Traffic"
        value={isLoading ? '—' : formatBytes((stats?.bytes_in ?? 0) + (stats?.bytes_out ?? 0))}
        icon={<ArrowDownUp className="h-4 w-4" />}
      />
    </div>
  );
}

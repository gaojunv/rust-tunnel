import { StatCard } from '@/components/shared/StatCard';
import { Network, Activity, Link, ArrowDown } from 'lucide-react';
import { useProxyStats } from '@/api/hooks';

export function ProxyStatsCards() {
  const { data: stats, isLoading } = useProxyStats();

  const formatBytes = (bytes: number) => {
    if (bytes === 0) return '0 B';
    const k = 1024;
    const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
  };

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
        icon={<ArrowDown className="h-4 w-4" />}
      />
    </div>
  );
}

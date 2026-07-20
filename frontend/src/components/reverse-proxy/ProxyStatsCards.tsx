import { StatCard } from '@/components/shared/StatCard';
import { Network, Activity, Link, ArrowDownUp } from 'lucide-react';
import { useProxyRules, useStatsSummary } from '@/api/hooks';
import { formatBytes } from '@/utils/format';

export function ProxyStatsCards() {
  const { data: summary, isLoading } = useStatsSummary();
  const { data: rules = [] } = useProxyRules();
  const stats = summary?.proxy;

  const activeRules = rules.filter((r) => r.enabled).length;

  return (
    <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-4">
      <StatCard
        title="Total Rules"
        value={isLoading ? '—' : rules.length}
        icon={<Network className="h-4 w-4" />}
      />
      <StatCard
        title="Active Rules"
        value={isLoading ? '—' : activeRules}
        icon={<Activity className="h-4 w-4" />}
      />
      <StatCard
        title="Total Connections"
        value={isLoading ? '—' : (stats?.total_conns ?? 0)}
        icon={<Link className="h-4 w-4" />}
      />
      <StatCard
        title="Total Traffic"
        value={isLoading ? '—' : formatBytes((stats?.total_bytes_in ?? 0) + (stats?.total_bytes_out ?? 0))}
        icon={<ArrowDownUp className="h-4 w-4" />}
      />
    </div>
  );
}

import { useTranslation } from 'react-i18next';
import { StatCard } from '@/components/shared/StatCard';
import { Network, Activity, Link, ArrowDownUp } from 'lucide-react';
import { useProxyRules, useStatsSummary } from '@/api/hooks';
import { formatBytes } from '@/utils/format';

export function ProxyStatsCards() {
  const { t } = useTranslation();
  const { data: summary, isLoading } = useStatsSummary();
  const { data: rules = [] } = useProxyRules();
  const proxyRules = rules.filter((r) => r.id !== '__llm_gateway__');
  const stats = summary?.proxy;

  const activeRules = proxyRules.filter((r) => r.enabled).length;

  return (
    <div className="grid grid-cols-1 gap-4 md:grid-cols-2 lg:grid-cols-4">
      <StatCard
        title={t('reverseProxy.stats.totalRules')}
        value={isLoading ? '—' : proxyRules.length}
        icon={<Network className="h-4 w-4" />}
      />
      <StatCard
        title={t('reverseProxy.stats.activeRules')}
        value={isLoading ? '—' : activeRules}
        icon={<Activity className="h-4 w-4" />}
      />
      <StatCard
        title={t('reverseProxy.stats.totalConnections')}
        value={isLoading ? '—' : (stats?.total_conns ?? 0)}
        icon={<Link className="h-4 w-4" />}
      />
      <StatCard
        title={t('reverseProxy.stats.totalTraffic')}
        value={isLoading ? '—' : formatBytes((stats?.total_bytes_in ?? 0) + (stats?.total_bytes_out ?? 0))}
        icon={<ArrowDownUp className="h-4 w-4" />}
      />
    </div>
  );
}

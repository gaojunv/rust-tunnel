import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import {
  api,
  login,
  getMetrics,
  getPortQuality,
  getPortTraffic,
  getTraffic,
  getAllQuality,
  getShadowsocksConfig,
  updateShadowsocksConfig,
  getShadowsocksStats,
  getShadowsocksQuality,
  getTrojanConfig,
  updateTrojanConfig,
  getTrojanStats,
  getTrojanQuality,
  getLogs,
  setLogsLevel,
  getProxyRules,
  createProxyRule,
  updateProxyRule,
  deleteProxyRule,
  getProxyStats,
  getAcmeStatus,
  getAcmeConfig,
  updateAcmeConfig,
  listAcmeCertificates,
  requestAcmeCertificate,
  renewAcmeCertificate,
  deleteAcmeCertificate,
  getDnsProviders,
  updateDnsProvider,
  getChallengeStatus,
  getSettings,
  getReverseProxyConfig,
  updateReverseProxyConfig,
  getDnsConfig,
  updateDnsConfig,
} from './client';
import type {
  LoginRequest,
  CreateProxyRuleRequest,
  UpdateProxyRuleRequest,
  UpdateAcmeConfigRequest,
  DnsProviderConfig,
  ReverseProxySettings,
  DnsSettings,
} from '../types';

export function useMetrics() {
  return useQuery({
    queryKey: ['metrics'],
    queryFn: () => getMetrics(),
    refetchInterval: 5000,
  });
}

export function useQuality(port: number) {
  return useQuery({
    queryKey: ['quality', port],
    queryFn: () => getPortQuality(port),
    enabled: port > 0,
    refetchInterval: 10000,
  });
}

export function useTraffic(port: number, _hours = 24) {
  return useQuery({
    queryKey: ['traffic', port, _hours],
    queryFn: () => getPortTraffic(port),
    enabled: port > 0,
  });
}

export function useAllTraffic() {
  return useQuery({
    queryKey: ['traffic', 'all'],
    queryFn: () => getTraffic(),
    refetchInterval: 30000,
  });
}

export function useQualitySummary() {
  return useQuery({
    queryKey: ['quality-summary'],
    queryFn: async () => {
      const clients = await getAllQuality();
      const totalConnections = clients.length;
      const warningCount = clients.filter(
        (c) => c.quality.is_warning || c.quality.is_critical
      ).length;
      const averageScore =
        totalConnections > 0
          ? clients.reduce((sum, c) => sum + c.quality.quality_score, 0) / totalConnections
          : 0;

      const mappedClients = clients.map((c) => ({
        port: c.port,
        hostname: c.hostname,
        score: c.quality.quality_score,
        rtt: c.quality.avg_rtt_ms,
        loss: c.quality.loss_rate * 100,
        is_warning: c.quality.is_warning,
        is_critical: c.quality.is_critical,
      }));

      const worst = [...mappedClients]
        .sort((a, b) => a.score - b.score)
        .slice(0, 10);

      return {
        total_connections: totalConnections,
        warning_count: warningCount,
        average_score: averageScore,
        clients: mappedClients,
        worst,
      };
    },
    refetchInterval: 10000,
  });
}

// Shadowsocks hooks
export function useShadowsocksConfig() {
  return useQuery({
    queryKey: ['shadowsocks-config'],
    queryFn: () => getShadowsocksConfig(),
  });
}

export function useUpdateShadowsocksConfig() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (config: { enabled: boolean; port: number; cipher: string }) =>
      updateShadowsocksConfig(config),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['shadowsocks-config'] });
      queryClient.invalidateQueries({ queryKey: ['shadowsocks-stats'] });
    },
  });
}

export function useShadowsocksStats() {
  return useQuery({
    queryKey: ['shadowsocks-stats'],
    queryFn: () => getShadowsocksStats(),
    refetchInterval: 5000,
  });
}

export function useShadowsocksQuality() {
  return useQuery({
    queryKey: ['shadowsocks-quality'],
    queryFn: () => getShadowsocksQuality(),
    refetchInterval: 10000,
  });
}

// Trojan hooks
export function useTrojanConfig() {
  return useQuery({
    queryKey: ['trojan-config'],
    queryFn: () => getTrojanConfig(),
  });
}

export function useUpdateTrojanConfig() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (config: {
      enabled: boolean;
      port: number;
      password?: string;
      fallback?: string;
      domain?: string;
    }) => updateTrojanConfig(config),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['trojan-config'] });
      queryClient.invalidateQueries({ queryKey: ['trojan-stats'] });
    },
  });
}

export function useTrojanStats() {
  return useQuery({
    queryKey: ['trojan-stats'],
    queryFn: () => getTrojanStats(),
    refetchInterval: 5000,
  });
}

export function useTrojanQuality() {
  return useQuery({
    queryKey: ['trojan-quality'],
    queryFn: () => getTrojanQuality(),
    refetchInterval: 10000,
  });
}

// Logs hooks
export function useLogs(params?: {
  level?: string;
  source?: string;
  search?: string;
  limit?: number;
  before_id?: number;
}) {
  return useQuery({
    queryKey: ['logs', params],
    queryFn: () => getLogs(params),
  });
}

export function useSetLogsLevel() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (level: string) => setLogsLevel(level),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['logs'] });
    },
  });
}

// Reverse Proxy hooks
export function useProxyRules() {
  return useQuery({
    queryKey: ['proxy-rules'],
    queryFn: () => getProxyRules(),
  });
}

export function useProxyStats() {
  return useQuery({
    queryKey: ['proxy-stats'],
    queryFn: () => getProxyStats(),
    refetchInterval: 10000,
  });
}

export function useCreateProxyRule() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (data: CreateProxyRuleRequest) => createProxyRule(data),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['proxy-rules'] });
      queryClient.invalidateQueries({ queryKey: ['proxy-stats'] });
    },
  });
}

export function useUpdateProxyRule() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ id, data }: { id: string; data: UpdateProxyRuleRequest }) =>
      updateProxyRule(id, data),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['proxy-rules'] });
      queryClient.invalidateQueries({ queryKey: ['proxy-stats'] });
    },
  });
}

export function useDeleteProxyRule() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => deleteProxyRule(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['proxy-rules'] });
      queryClient.invalidateQueries({ queryKey: ['proxy-stats'] });
    },
  });
}

// ACME hooks
export function useAcmeStatus() {
  return useQuery({
    queryKey: ['acme-status'],
    queryFn: () => getAcmeStatus(),
  });
}

export function useAcmeConfig() {
  return useQuery({
    queryKey: ['acme-config'],
    queryFn: () => getAcmeConfig(),
  });
}

export function useUpdateAcmeConfig() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (data: UpdateAcmeConfigRequest) => updateAcmeConfig(data),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['acme-config'] });
      queryClient.invalidateQueries({ queryKey: ['acme-status'] });
    },
  });
}

export function useAcmeCertificates() {
  return useQuery({
    queryKey: ['acme-certificates'],
    queryFn: () => listAcmeCertificates(),
  });
}

export function useRequestAcmeCertificate() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ domain, challengeType }: { domain: string; challengeType: string }) =>
      requestAcmeCertificate(domain, challengeType),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['acme-certificates'] });
    },
  });
}

export function useRenewAcmeCertificate() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (domain: string) => renewAcmeCertificate(domain),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['acme-certificates'] });
    },
  });
}

export function useDeleteAcmeCertificate() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (domain: string) => deleteAcmeCertificate(domain),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['acme-certificates'] });
    },
  });
}

// ACME DNS Provider hooks
export function useDnsProviders() {
  return useQuery({
    queryKey: ['dns-providers'],
    queryFn: () => getDnsProviders(),
  });
}

export function useUpdateDnsProvider() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (config: DnsProviderConfig) => updateDnsProvider(config),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['dns-providers'] });
      queryClient.invalidateQueries({ queryKey: ['acme-config'] });
    },
  });
}

export function useChallengeStatus(domain: string) {
  return useQuery({
    queryKey: ['challenge-status', domain],
    queryFn: () => getChallengeStatus(domain),
    enabled: !!domain,
    refetchInterval: 5000,
  });
}

export function useLogin() {
  return useMutation({
    mutationFn: (password: string) => login({ password } as LoginRequest),
  });
}

// Settings hooks
export function useSettings() {
  return useQuery({
    queryKey: ['settings'],
    queryFn: () => getSettings(),
  });
}

export function useReverseProxyConfig() {
  return useQuery({
    queryKey: ['settings', 'reverse-proxy'],
    queryFn: () => getReverseProxyConfig(),
  });
}

export function useUpdateReverseProxyConfig() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (config: ReverseProxySettings) => updateReverseProxyConfig(config),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['settings'] });
    },
  });
}

export function useDnsConfig() {
  return useQuery({
    queryKey: ['settings', 'dns'],
    queryFn: () => getDnsConfig(),
  });
}

export function useUpdateDnsConfig() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (config: DnsSettings) => updateDnsConfig(config),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['settings', 'dns'] });
    },
  });
}

// ── Unified Stats Hooks ─────────────────────────────────────────

import { statsStream } from './statsStream';
import type { StatsSnapshot, StatsSummary } from '@/types';
import { useEffect } from 'react';

export function useStatsQuery(
  entityType?: string[],
  entityId?: string[],
  start?: string,
  end?: string,
) {
  return useQuery({
    queryKey: ['stats', 'query', entityType, entityId, start, end],
    queryFn: async () => {
      const params = new URLSearchParams();
      entityType?.forEach((et) => params.append('entity_type', et));
      entityId?.forEach((eid) => params.append('entity_id', eid));
      if (start) params.set('start', start);
      if (end) params.set('end', end);
      const res = await api.get<{ snapshots: StatsSnapshot[] }>(`/api/stats/query?${params}`);
      return res.data.snapshots;
    },
    enabled: !!start && !!end,
  });
}

export function useStatsSummary() {
  return useQuery({
    queryKey: ['stats', 'summary'],
    queryFn: async () => {
      const res = await api.get<StatsSummary>('/api/stats/summary');
      return res.data;
    },
    refetchInterval: 60_000,
  });
}

export function useStatsStream(entityType?: string) {
  const queryClient = useQueryClient();

  useEffect(() => {
    const unsub = statsStream.subscribe(entityType, (snap) => {
      queryClient.setQueryData(['stats', 'summary'], (old: StatsSummary | undefined) => {
        if (!old) return old;
        const key = snap.entity_type === 'client' ? 'clients' : snap.entity_type;
        const bucket = old[key as keyof StatsSummary];
        if (!bucket) return old;
        return {
          ...old,
          [key]: {
            ...bucket,
            total_bytes_in: Math.max(bucket.total_bytes_in, snap.bytes_in),
            total_bytes_out: Math.max(bucket.total_bytes_out, snap.bytes_out),
            total_conns: snap.active_conns,
          },
        };
      });
    });
    return unsub;
  }, [entityType, queryClient]);
}

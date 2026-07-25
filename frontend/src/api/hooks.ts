import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import {
  api,
  login,
  getShadowsocksConfig,
  updateShadowsocksConfig,
  getTrojanConfig,
  updateTrojanConfig,
  getLogs,
  setLogsLevel,
  getProxyRules,
  createProxyRule,
  updateProxyRule,
  deleteProxyRule,
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
  getLlmGatewayConfig,
  updateLlmGatewayConfig,
  listLlmProviders,
  createLlmProvider,
  updateLlmProvider,
  toggleLlmProvider,
  deleteLlmProvider,
  listProviderModels,
  addModel,
  updateModel,
  deleteModel,
  listLlmApiKeys,
  createLlmApiKey,
  toggleLlmApiKey,
  deleteLlmApiKey,
  getLlmUsageSummary,
  getLlmUsageAggregate,
  getLlmUsageLogs,
} from './client';
import type {
  LoginRequest,
  CreateProxyRuleRequest,
  UpdateProxyRuleRequest,
  UpdateAcmeConfigRequest,
  DnsProviderConfig,
  ReverseProxySettings,
  DnsSettings,
  CreateProviderRequest,
  CreateModelRequest,
  LlmGatewayConfig,
  UsageGroupBy,
} from '../types';

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
      queryClient.invalidateQueries({ queryKey: ['stats'] });
    },
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
      queryClient.invalidateQueries({ queryKey: ['stats'] });
    },
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

export function useCreateProxyRule() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (data: CreateProxyRuleRequest) => createProxyRule(data),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['proxy-rules'] });
      queryClient.invalidateQueries({ queryKey: ['stats'] });
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
      queryClient.invalidateQueries({ queryKey: ['stats'] });
    },
  });
}

export function useDeleteProxyRule() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => deleteProxyRule(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['proxy-rules'] });
      queryClient.invalidateQueries({ queryKey: ['stats'] });
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
import { useEffect, useRef } from 'react';

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
      const res = await api.get<{ snapshots: StatsSnapshot[] }>(`/stats/query?${params}`);
      return res.data.snapshots;
    },
    enabled: !!start && !!end,
  });
}

export function useStatsSummary() {
  return useQuery({
    queryKey: ['stats', 'summary'],
    queryFn: async () => {
      const res = await api.get<StatsSummary>('/stats/summary');
      return res.data;
    },
    refetchInterval: 60_000,
  });
}

export function useStatsStream(entityType?: string) {
  const queryClient = useQueryClient();
  const lastInvalidateRef = useRef(0);

  useEffect(() => {
    // SSE 快照为单实体粒度，直接覆盖聚合桶会得到错误总数。
    // 改为节流 invalidate：距上次 ≥10s 才让 summary 从服务端重新聚合。
    const unsub = statsStream.subscribe(entityType, () => {
      const now = Date.now();
      if (now - lastInvalidateRef.current >= 10_000) {
        lastInvalidateRef.current = now;
        queryClient.invalidateQueries({ queryKey: ['stats', 'summary'] });
      }
    });
    return unsub;
  }, [entityType, queryClient]);
}

// ── LLM Gateway ──────────────────────────────────────────────

export function useLlmGatewayConfig() {
  return useQuery({
    queryKey: ['llm-gateway-config'],
    queryFn: () => getLlmGatewayConfig(),
  });
}

export function useUpdateLlmGatewayConfig() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (config: Partial<LlmGatewayConfig>) => updateLlmGatewayConfig(config),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['llm-gateway-config'] }),
  });
}

// ── Providers ────────────────────────────────────────────────

export function useLlmProviders() {
  return useQuery({ queryKey: ['llm-providers'], queryFn: () => listLlmProviders() });
}

export function useCreateLlmProvider() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: CreateProviderRequest) => createLlmProvider(req),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['llm-providers'] }),
  });
}

export function useUpdateLlmProvider() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, ...req }: { id: string } & CreateProviderRequest) => updateLlmProvider(id, req),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['llm-providers'] }),
  });
}

export function useToggleLlmProvider() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, enabled }: { id: string; enabled: boolean }) => toggleLlmProvider(id, enabled),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['llm-providers'] }),
  });
}

export function useDeleteLlmProvider() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => deleteLlmProvider(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['llm-providers'] }),
  });
}

// ── Models ───────────────────────────────────────────────────

export function useProviderModels(providerId: string) {
  return useQuery({
    queryKey: ['llm-models', providerId],
    queryFn: () => listProviderModels(providerId),
    enabled: !!providerId,
  });
}

export function useAddModel() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ providerId, ...req }: { providerId: string } & CreateModelRequest) => addModel(providerId, req),
    onSuccess: (_data, vars) => qc.invalidateQueries({ queryKey: ['llm-models', vars.providerId] }),
  });
}

export function useUpdateModel() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, ...req }: { id: string } & CreateModelRequest) => updateModel(id, req),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['llm-models'] }),
  });
}

export function useDeleteModel() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => deleteModel(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['llm-models'] }),
  });
}

// ── API Keys ─────────────────────────────────────────────────

export function useLlmApiKeys() {
  return useQuery({ queryKey: ['llm-api-keys'], queryFn: () => listLlmApiKeys() });
}

export function useCreateLlmApiKey() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => createLlmApiKey(name),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['llm-api-keys'] }),
  });
}

export function useToggleLlmApiKey() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, enabled }: { id: string; enabled: boolean }) => toggleLlmApiKey(id, enabled),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['llm-api-keys'] }),
  });
}

export function useDeleteLlmApiKey() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => deleteLlmApiKey(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['llm-api-keys'] }),
  });
}

// ── Usage stats ──────────────────────────────────────────────

interface UsageRange {
  start: string;
  end: string;
}

export function useLlmUsageSummary(range: UsageRange) {
  return useQuery({
    queryKey: ['llm-usage-summary', range.start, range.end],
    queryFn: () => getLlmUsageSummary(range),
  });
}

export function useLlmUsageAggregate(groupBy: UsageGroupBy, range: UsageRange) {
  return useQuery({
    queryKey: ['llm-usage-aggregate', groupBy, range.start, range.end],
    queryFn: () => getLlmUsageAggregate(groupBy, range),
  });
}

export function useLlmUsageLogs(range: UsageRange, limit = 50, offset = 0) {
  return useQuery({
    queryKey: ['llm-usage-logs', range.start, range.end, limit, offset],
    queryFn: () => getLlmUsageLogs({ ...range, limit, offset }),
  });
}

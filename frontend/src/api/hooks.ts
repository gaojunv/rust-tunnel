import { keepPreviousData, useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import {
  api,
  login,
  getShadowsocksConfig,
  updateShadowsocksConfig,
  getTrojanConfig,
  updateTrojanConfig,
  getLogs,
  setLogsLevel,
  getLlmLogging,
  setLlmLogging,
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
  bindLlmApiKey,
  deleteLlmApiKey,
  getLlmUsageSummary,
  getLlmUsageAggregate,
  getLlmUsageLogs,
  listAllLlmModels,
  listLlmModelGroups,
  createLlmModelGroup,
  getLlmModelGroup,
  updateLlmModelGroup,
  deleteLlmModelGroup,
  replaceGroupMembers,
  resetGroupBreaker,
  listLlmKbs,
  getLlmKb,
  createLlmKb,
  updateLlmKb,
  toggleLlmKb,
  deleteLlmKb,
  listLlmKbDocs,
  uploadKbDoc,
  deleteKbDoc,
  reindexKbDoc,
  testEmbedding,
  queryKb,
  clientsApi,
  listAgentWorkspaces,
  getMemorySettings,
  updateMemorySettings,
  testMemoryEmbedding,
  clearMemory,
  listMemories,
  createMemory,
  updateMemory,
  deleteMemory,
  pinMemory,
  listSkills,
  getSkill,
  createSkill,
  updateSkill,
  deleteSkill,
  toggleSkill,
  listRoles,
  createRole,
  updateRole,
  deleteRole,
  toggleRole,
  updateAgentSessionRole,
  listWikis,
  createWiki,
  updateWiki,
  deleteWiki,
  listWikiDocs,
  uploadWikiDoc,
  deleteWikiDoc,
  reindexWikiDoc,
  listWikiPages,
  getWikiPage,
  putWikiPage,
  deleteWikiPage,
  searchWiki,
  searchAllWikis,
  getWikiGraph,
  type MemoryListParams,
  type SkillListParams,
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
  CreateLlmKbRequest,
  UpdateLlmKbRequest,
  MemorySettingsRequest,
  CreateMemoryRequest,
  UpdateMemoryRequest,
  CreateSkillRequest,
  UpdateSkillRequest,
  CreateRoleRequest,
  UpdateRoleRequest,
  RoleListParams,
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

export function useLlmLogging() {
  const queryClient = useQueryClient();
  const query = useQuery({
    queryKey: ['llm-logging'],
    queryFn: getLlmLogging,
  });
  const mutation = useMutation({
    mutationFn: setLlmLogging,
    onMutate: async (enabled: boolean) => {
      // 取消进行中的查询，避免乐观更新与后台拉取竞态
      await queryClient.cancelQueries({ queryKey: ['llm-logging'] });
      // 乐观更新：立即切换 UI，慢网络下不回跳
      queryClient.setQueryData<{ enabled: boolean }>(['llm-logging'], { enabled });
    },
    onError: () => {
      // 回滚：失效缓存重新拉取真实值
      queryClient.invalidateQueries({ queryKey: ['llm-logging'] });
    },
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['llm-logging'] }),
  });
  return { ...query, setLlmLogging: mutation.mutate, isToggling: mutation.isPending };
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
import { memoryStream } from './memoryStream';
import { wikiStream } from './wikiStream';
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

/** 全部模型（GET /api/llm/models），用于模型组选模型。 */
export function useLlmAllModels() {
  return useQuery({ queryKey: ['llm-models', 'all'], queryFn: () => listAllLlmModels() });
}

export function useAddModel() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ providerId, ...req }: { providerId: string } & CreateModelRequest) => addModel(providerId, req),
    // 新增模型后需刷新该 provider 的列表以及全部模型列表（GroupDialog 用 ['llm-models','all']）。
    // 仅刷新 ['llm-models', providerId] 会让 GroupDialog 的选模型下拉陈旧。
    onSuccess: () => qc.invalidateQueries({ queryKey: ['llm-models'] }),
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

export function useBindLlmApiKey() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, kbId }: { id: string; kbId: string | null }) => bindLlmApiKey(id, kbId),
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

// ── 模型组（多模型故障转移） ──────────────────────────────────

export function useLlmModelGroups() {
  return useQuery({ queryKey: ['llm-model-groups'], queryFn: () => listLlmModelGroups() });
}

export function useLlmModelGroup(id: string | undefined) {
  return useQuery({
    queryKey: ['llm-model-groups', id],
    queryFn: () => getLlmModelGroup(id!),
    enabled: !!id,
    refetchInterval: 5000, // 熔断状态轮询刷新
    refetchIntervalInBackground: false, // 页签后台时暂停轮询
  });
}

export function useCreateLlmModelGroup() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: { name: string; enabled?: boolean }) => createLlmModelGroup(req),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['llm-model-groups'] }),
  });
}

export function useUpdateLlmModelGroup() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, ...req }: { id: string; name: string; enabled?: boolean }) =>
      updateLlmModelGroup(id, req),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['llm-model-groups'] }),
  });
}

export function useDeleteLlmModelGroup() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => deleteLlmModelGroup(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['llm-model-groups'] }),
  });
}

export function useReplaceGroupMembers() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, members }: { id: string; members: { model_id: string; priority: number }[] }) =>
      replaceGroupMembers(id, members),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['llm-model-groups'] }),
  });
}

export function useResetGroupBreaker() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => resetGroupBreaker(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['llm-model-groups'] }),
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
    // 统计跟随时间滚动：时间窗冻结在挂载时刻会让「最近 N 小时」口径失真
    refetchInterval: 30_000,
  });
}

export function useLlmUsageAggregate(groupBy: UsageGroupBy, range: UsageRange) {
  return useQuery({
    queryKey: ['llm-usage-aggregate', groupBy, range.start, range.end],
    queryFn: () => getLlmUsageAggregate(groupBy, range),
    refetchInterval: 30_000,
  });
}

export function useLlmUsageLogs(range: UsageRange, limit = 50, offset = 0) {
  return useQuery({
    queryKey: ['llm-usage-logs', range.start, range.end, limit, offset],
    queryFn: () => getLlmUsageLogs({ ...range, limit, offset }),
    refetchInterval: 30_000,
    // 翻页时保留上一页数据，避免整表闪回 loading 行
    placeholderData: keepPreviousData,
  });
}

// ── RAG Knowledge Base ──────────────────────────────────────

export function useLlmKbs() {
  return useQuery({ queryKey: ['llm-kbs'], queryFn: () => listLlmKbs() });
}

export function useLlmKb(id: string) {
  return useQuery({
    queryKey: ['llm-kb', id],
    queryFn: () => getLlmKb(id),
    enabled: !!id,
  });
}

export function useCreateLlmKb() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: CreateLlmKbRequest) => createLlmKb(req),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['llm-kbs'] }),
  });
}

export function useUpdateLlmKb() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, ...req }: { id: string } & UpdateLlmKbRequest) => updateLlmKb(id, req),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ['llm-kbs'] });
      qc.invalidateQueries({ queryKey: ['llm-kb', vars.id] });
    },
  });
}

export function useToggleLlmKb() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, enabled }: { id: string; enabled: boolean }) => toggleLlmKb(id, enabled),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ['llm-kbs'] });
      qc.invalidateQueries({ queryKey: ['llm-kb', vars.id] });
    },
  });
}

export function useDeleteLlmKb() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => deleteLlmKb(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['llm-kbs'] }),
  });
}

export function useLlmKbDocs(kbId: string) {
  return useQuery({
    queryKey: ['llm-kb-docs', kbId],
    queryFn: () => listLlmKbDocs(kbId),
    enabled: !!kbId,
  });
}

export function useUploadKbDoc() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ kbId, file }: { kbId: string; file: File }) => uploadKbDoc(kbId, file),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ['llm-kb-docs', vars.kbId] });
      qc.invalidateQueries({ queryKey: ['llm-kbs'] });
    },
  });
}

export function useDeleteKbDoc() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ kbId, docId }: { kbId: string; docId: string }) => deleteKbDoc(kbId, docId),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ['llm-kb-docs', vars.kbId] });
      qc.invalidateQueries({ queryKey: ['llm-kbs'] });
    },
  });
}

export function useReindexKbDoc() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ kbId, docId }: { kbId: string; docId: string }) => reindexKbDoc(kbId, docId),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ['llm-kb-docs', vars.kbId] });
      qc.invalidateQueries({ queryKey: ['llm-kbs'] });
    },
  });
}

export function useTestEmbedding() {
  return useMutation({
    mutationFn: (req: { base_url: string; api_key: string; model: string; kb_id?: string }) =>
      testEmbedding(req),
  });
}

export function useKbQuery() {
  return useMutation({
    mutationFn: ({ kbId, text }: { kbId: string; text: string }) => queryKb(kbId, text),
  });
}

// ── Agent Memory ──────────────────────────────────────────────

/** 记忆列表查询参数（含 UI 过滤映射后的空值剔除）。queryKey `['agent-memories', params]`。 */
export function useMemories(params: MemoryListParams = {}) {
  return useQuery({
    queryKey: ['agent-memories', params],
    queryFn: () => listMemories(params),
  });
}

export function useCreateMemory() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: CreateMemoryRequest) => createMemory(req),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['agent-memories'] }),
  });
}

export function useUpdateMemory() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, ...req }: { id: string } & UpdateMemoryRequest) => updateMemory(id, req),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ['agent-memories'] });
      qc.invalidateQueries({ queryKey: ['agent-memory', vars.id] });
    },
  });
}

export function useDeleteMemory() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => deleteMemory(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['agent-memories'] }),
  });
}

export function usePinMemory() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => pinMemory(id),
    onSuccess: (_data, id) => {
      qc.invalidateQueries({ queryKey: ['agent-memories'] });
      qc.invalidateQueries({ queryKey: ['agent-memory', id] });
    },
  });
}

export function useMemorySettings() {
  return useQuery({ queryKey: ['agent-memory-settings'], queryFn: () => getMemorySettings() });
}

export function useUpdateMemorySettings() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: MemorySettingsRequest) => updateMemorySettings(req),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['agent-memory-settings'] }),
  });
}

export function useTestMemoryEmbedding() {
  return useMutation({
    mutationFn: (req: { base_url: string; api_key: string; model: string }) =>
      testMemoryEmbedding(req),
  });
}

export function useClearMemory() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => clearMemory(),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['agent-memories'] }),
  });
}

/** 作用域下拉数据源（复用既有缓存键，避免重复请求）。 */
export function useAgentWorkspaces() {
  return useQuery({ queryKey: ['agent-workspaces'], queryFn: () => listAgentWorkspaces() });
}

export function useClients() {
  return useQuery({ queryKey: ['clients'], queryFn: () => clientsApi.list() });
}

/** 订阅记忆/技能 SSE：事件到达即失效记忆与技能列表（后台重拉）。
 *  事件不带逐条 id，故无 KbDetail 的 override 通道，仅 invalidate。 */
export function useMemoryStream() {
  const qc = useQueryClient();
  useEffect(() => {
    const unsub = memoryStream.subscribe(() => {
      qc.invalidateQueries({ queryKey: ['agent-memories'] });
      qc.invalidateQueries({ queryKey: ['agent-skills'] });
    });
    return unsub;
  }, [qc]);
}

// ── Agent Skill ──────────────────────────────────────────────────

/** 技能列表查询参数（含 UI 过滤映射后的空值剔除）。queryKey `['agent-skills', params]`。 */
export function useSkills(params: SkillListParams = {}) {
  return useQuery({
    queryKey: ['agent-skills', params],
    queryFn: () => listSkills(params),
  });
}

/** 技能详情（含 content）。queryKey `['agent-skill', id]`。 */
export function useSkill(id: string) {
  return useQuery({
    queryKey: ['agent-skill', id],
    queryFn: () => getSkill(id),
    enabled: !!id,
  });
}

export function useCreateSkill() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: CreateSkillRequest) => createSkill(req),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['agent-skills'] }),
  });
}

export function useUpdateSkill() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, ...req }: { id: string } & UpdateSkillRequest) => updateSkill(id, req),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ['agent-skills'] });
      qc.invalidateQueries({ queryKey: ['agent-skill', vars.id] });
    },
  });
}

export function useDeleteSkill() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => deleteSkill(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['agent-skills'] }),
  });
}

export function useToggleSkill() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => toggleSkill(id),
    onSuccess: (_data, id) => {
      qc.invalidateQueries({ queryKey: ['agent-skills'] });
      qc.invalidateQueries({ queryKey: ['agent-skill', id] });
    },
  });
}

// ── Agent Role ──────────────────────────────────────────────────

/** 角色列表查询参数（含 UI 过滤映射后的空值剔除）。queryKey `['agent-roles', params]`。 */
export function useRoles(params: RoleListParams = {}) {
  return useQuery({
    queryKey: ['agent-roles', params],
    queryFn: () => listRoles(params),
  });
}

export function useCreateRole() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: CreateRoleRequest) => createRole(req),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['agent-roles'] }),
  });
}

export function useUpdateRole() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, ...req }: { id: string } & UpdateRoleRequest) => updateRole(id, req),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ['agent-roles'] });
      qc.invalidateQueries({ queryKey: ['agent-role', vars.id] });
    },
  });
}

export function useDeleteRole() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => deleteRole(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['agent-roles'] }),
  });
}

export function useToggleRole() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => toggleRole(id),
    onSuccess: (_data, id) => {
      qc.invalidateQueries({ queryKey: ['agent-roles'] });
      qc.invalidateQueries({ queryKey: ['agent-role', id] });
    },
  });
}

export function useUpdateSessionRole() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ sessionId, roleId }: { sessionId: string; roleId: string | null }) =>
      updateAgentSessionRole(sessionId, roleId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['agent-roles'] });
      // 会话列表缓存里带了 role_id（SessionSettingsMenu 当前角色回显的读取源），
      // 不失效则切换角色后菜单勾选不更新。
      qc.invalidateQueries({ queryKey: ['agent-sessions'] });
    },
  });
}

// ── Agent Wiki（批 4 完整） ───────────────────────────────────

/** Wiki 容器列表查询参数（含 UI 过滤映射后的空值剔除）。queryKey `['agent-wikis', params]`。 */
export function useWikis(params: import('./client').WikiListParams = {}) {
  return useQuery({
    queryKey: ['agent-wikis', params],
    queryFn: () => listWikis(params),
  });
}

export function useCreateWiki() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: import('../types').CreateWikiRequest) => createWiki(req),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['agent-wikis'] }),
  });
}

export function useUpdateWiki() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, ...req }: { id: string } & import('../types').UpdateWikiRequest) => updateWiki(id, req),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ['agent-wikis'] });
      qc.invalidateQueries({ queryKey: ['agent-wiki', vars.id] });
    },
  });
}

export function useDeleteWiki() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => deleteWiki(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['agent-wikis'] }),
  });
}

/** Wiki 文档列表。queryKey `['agent-wiki-docs', wikiId]`。 */
export function useWikiDocs(wikiId: string) {
  return useQuery({
    queryKey: ['agent-wiki-docs', wikiId],
    queryFn: () => listWikiDocs(wikiId),
    enabled: !!wikiId,
  });
}

export function useUploadWikiDoc() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ wikiId, file }: { wikiId: string; file: File }) => uploadWikiDoc(wikiId, file),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ['agent-wiki-docs', vars.wikiId] });
      qc.invalidateQueries({ queryKey: ['agent-wikis'] });
    },
  });
}

export function useDeleteWikiDoc() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ wikiId, docId }: { wikiId: string; docId: string }) => deleteWikiDoc(wikiId, docId),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ['agent-wiki-docs', vars.wikiId] });
      qc.invalidateQueries({ queryKey: ['agent-wikis'] });
    },
  });
}

export function useReindexWikiDoc() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ wikiId, docId }: { wikiId: string; docId: string }) => reindexWikiDoc(wikiId, docId),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ['agent-wiki-docs', vars.wikiId] });
      qc.invalidateQueries({ queryKey: ['agent-wikis'] });
    },
  });
}

/** Wiki 页面列表（含过滤参数）。queryKey `['agent-wiki-pages', wikiId, params]`。 */
export function useWikiPages(wikiId: string, params: import('../types').WikiPageListParams = {}) {
  return useQuery({
    queryKey: ['agent-wiki-pages', wikiId, params],
    queryFn: () => listWikiPages(wikiId, params),
    enabled: !!wikiId,
  });
}

/** 页面全文。queryKey `['agent-wiki-page', wikiId, ref]`；ref 为 null 时禁用。 */
export function useWikiPage(wikiId: string, ref: string | null) {
  return useQuery({
    queryKey: ['agent-wiki-page', wikiId, ref],
    queryFn: () => getWikiPage(wikiId, ref!),
    enabled: !!wikiId && !!ref,
  });
}

export function usePutWikiPage() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ wikiId, ref, req }: { wikiId: string; ref: string; req: import('../types').PutWikiPageRequest }) =>
      putWikiPage(wikiId, ref, req),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ['agent-wiki-pages', vars.wikiId] });
      qc.invalidateQueries({ queryKey: ['agent-wiki-page', vars.wikiId, vars.req.ref ?? vars.ref] });
      qc.invalidateQueries({ queryKey: ['agent-wiki-graph', vars.wikiId] });
      qc.invalidateQueries({ queryKey: ['agent-wikis'] });
    },
  });
}

export function useDeleteWikiPage() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ wikiId, ref }: { wikiId: string; ref: string }) => deleteWikiPage(wikiId, ref),
    onSuccess: (_data, vars) => {
      qc.invalidateQueries({ queryKey: ['agent-wiki-pages', vars.wikiId] });
      qc.invalidateQueries({ queryKey: ['agent-wiki-page', vars.wikiId, vars.ref] });
      qc.invalidateQueries({ queryKey: ['agent-wiki-graph', vars.wikiId] });
      qc.invalidateQueries({ queryKey: ['agent-wikis'] });
    },
  });
}

/** Wiki 图谱数据。queryKey `['agent-wiki-graph', wikiId]`。 */
export function useWikiGraph(wikiId: string) {
  return useQuery({
    queryKey: ['agent-wiki-graph', wikiId],
    queryFn: () => getWikiGraph(wikiId),
    enabled: !!wikiId,
  });
}

/** 单容器搜索（BM25+LIKE 回退）。q 为空时禁用。queryKey `['agent-wiki-search', wikiId, q]`。 */
export function useWikiSearch(wikiId: string, q: string) {
  return useQuery({
    queryKey: ['agent-wiki-search', wikiId, q],
    queryFn: () => searchWiki(wikiId, q),
    enabled: !!wikiId && !!q,
  });
}

/** 跨容器搜索（`/api/agent/wiki/search`）。q 为空时禁用。queryKey `['agent-wiki-search-all', q]`。 */
export function useSearchAllWikis(q: string) {
  return useQuery({
    queryKey: ['agent-wiki-search-all', q],
    queryFn: () => searchAllWikis(q),
    enabled: !!q,
  });
}

/** 订阅 Wiki 摄入 SSE：doc 状态事件到达即失效文档列表 + 容器 + 图谱。
 *  事件不带逐条文档详情，无 KbDetail 的 override 通道（状态实时性靠文档列表 refetch）。
 *  返回订阅函数供 WikiDetail 做 override 细化（仿 useMemoryStream 前置用法）。 */
export function useWikiStream() {
  const qc = useQueryClient();
  useEffect(() => {
    const unsub = wikiStream.subscribe({
      onWiki: () => {
        qc.invalidateQueries({ queryKey: ['agent-wiki-docs'] });
        qc.invalidateQueries({ queryKey: ['agent-wikis'] });
        qc.invalidateQueries({ queryKey: ['agent-wiki-graph'] });
      },
      onSync: () => {
        // Lagged：广播槽溢出丢事件，强制重拉列表以获得完整状态
        qc.invalidateQueries({ queryKey: ['agent-wiki-docs'] });
        qc.invalidateQueries({ queryKey: ['agent-wikis'] });
        qc.invalidateQueries({ queryKey: ['agent-wiki-graph'] });
      },
    });
    return unsub;
  }, [qc]);
}

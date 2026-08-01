import axios from 'axios';
import type {
  LoginRequest,
  LoginResponse,
  ShadowsocksConfig,
  TrojanConfig,
  LogEntry,
  MeshNetworkResponse,
  MeshServiceResponse,
  DnsRecordResponse,
  AddDnsRecordRequest,
  ProxyRule,
  CreateProxyRuleRequest,
  UpdateProxyRuleRequest,
  AcmeStatus,
  AcmeCertificate,
  AcmeConfig,
  UpdateAcmeConfigRequest,
  DnsProviderConfig,
  ChallengeStatus,
  GeneralSettings,
  ReverseProxySettings,
  DnsSettings,
  Client,
  ServerAuthView,
  LlmProvider,
  CreateProviderRequest,
  LlmModel,
  CreateModelRequest,
  LlmApiKey,
  CreateApiKeyResponse,
  LlmGatewayConfig,
  LlmUsageSummary,
  LlmUsageAggregateRow,
  LlmUsageLogsResponse,
  UsageGroupBy,
  LlmKnowledgeBase,
  LlmKbDocument,
  KbQueryResult,
  CreateLlmKbRequest,
  UpdateLlmKbRequest,
  TestEmbeddingResult,
} from '../types';

const API_BASE = '/api';

// Create axios instance
export const api = axios.create({
  baseURL: API_BASE,
});

// Add auth token to requests
api.interceptors.request.use((config) => {
  const token = localStorage.getItem('auth_token');
  if (token) {
    config.headers.Authorization = `Bearer ${token}`;
  }
  return config;
});

// Handle 401 responses
api.interceptors.response.use(
  (response) => response,
  (error) => {
    if (error.response?.status === 401) {
      localStorage.removeItem('auth_token');
      window.location.href = '/';
    }
    return Promise.reject(error);
  }
);

// Auth API
export const login = async (data: LoginRequest): Promise<LoginResponse> => {
  const response = await api.post<LoginResponse>('/login', data);
  if (response.data.token) {
    localStorage.setItem('auth_token', response.data.token);
  }
  return response.data;
};

export const logout = async (): Promise<void> => {
  await api.post('/logout');
  localStorage.removeItem('auth_token');
};

// Health check
export const checkHealth = async (): Promise<{ status: string }> => {
  const response = await api.get('/health');
  return response.data;
};

// Check if we have a token
export const isAuthenticated = (): boolean => {
  return !!localStorage.getItem('auth_token');
};

// Shadowsocks API
export const getShadowsocksConfig = async (): Promise<ShadowsocksConfig> => {
  const response = await api.get<ShadowsocksConfig>('/shadowsocks');
  return response.data;
};

export const updateShadowsocksConfig = async (config: Partial<ShadowsocksConfig>): Promise<ShadowsocksConfig> => {
  const response = await api.post<ShadowsocksConfig>('/shadowsocks', config);
  return response.data;
};

// Trojan API
export const getTrojanConfig = async (): Promise<TrojanConfig> => {
  const response = await api.get<TrojanConfig>('/trojan');
  return response.data;
};

export const updateTrojanConfig = async (config: Partial<TrojanConfig>): Promise<TrojanConfig> => {
  const response = await api.post<TrojanConfig>('/trojan', config);
  return response.data;
};

// Logs API
export const getLogs = async (params?: {
  level?: string;
  source?: string;
  search?: string;
  limit?: number;
  before_id?: number;
}): Promise<LogEntry[]> => {
  const response = await api.get<LogEntry[]>('/logs', { params });
  return response.data;
};

export const getLogsLevel = async (): Promise<{ level: string }> => {
  const response = await api.get<{ level: string }>('/logs/level');
  return response.data;
};

export const setLogsLevel = async (level: string): Promise<{ level: string }> => {
  const response = await api.put<{ level: string }>('/logs/level', { level });
  return response.data;
};

export const getLlmLogging = async (): Promise<{ enabled: boolean }> => {
  const response = await api.get<{ enabled: boolean }>('/logs/llm-logging');
  return response.data;
};

export const setLlmLogging = async (enabled: boolean): Promise<{ enabled: boolean }> => {
  const response = await api.put<{ enabled: boolean }>('/logs/llm-logging', { enabled });
  return response.data;
};

// Mesh API
export const getMeshes = async (): Promise<MeshNetworkResponse[]> => {
  const response = await api.get<MeshNetworkResponse[]>('/mesh');
  return response.data;
};

export const getMesh = async (id: string): Promise<MeshNetworkResponse> => {
  const response = await api.get<MeshNetworkResponse>(`/mesh/${id}`);
  return response.data;
};

export const getMeshServices = async (id: string): Promise<MeshServiceResponse[]> => {
  const response = await api.get<MeshServiceResponse[]>(`/mesh/${id}/services`);
  return response.data;
};

// DNS API
export const getDnsRecords = async (): Promise<DnsRecordResponse[]> => {
  const response = await api.get<DnsRecordResponse[]>('/dns/records');
  return response.data;
};

export const addDnsRecord = async (record: AddDnsRecordRequest): Promise<void> => {
  await api.post('/dns/records', record);
};

export const deleteDnsRecord = async (name: string): Promise<void> => {
  await api.delete(`/dns/records/${encodeURIComponent(name)}`);
};

// Clients API (v2)
export const clientsApi = {
  list: async (): Promise<Client[]> => {
    const { data } = await api.get<{ clients: Client[] }>('/clients');
    return data.clients;
  },
  patchNote: async (name: string, note: string | null): Promise<void> => {
    await api.patch(`/clients/${encodeURIComponent(name)}`, { note });
  },
  remove: async (name: string): Promise<void> => {
    await api.delete(`/clients/${encodeURIComponent(name)}`);
  },
  kick: async (name: string): Promise<void> => {
    await api.post(`/clients/${encodeURIComponent(name)}/kick`);
  },
};

export const serverAuthApi = {
  get: async (): Promise<ServerAuthView> => {
    const { data } = await api.get<ServerAuthView>('/server-auth');
    return data;
  },
  rotate: async (): Promise<string> => {
    const { data } = await api.post<{ client_token: string }>('/server-auth/rotate');
    return data.client_token;
  },
  set: async (token: string): Promise<void> => {
    await api.put('/server-auth', { token });
  },
};

// Reverse Proxy API
export const getProxyRules = async (): Promise<ProxyRule[]> => {
  const response = await api.get<{ rules: ProxyRule[] }>('/proxy/rules');
  return response.data.rules;
};

export const createProxyRule = async (data: CreateProxyRuleRequest): Promise<ProxyRule> => {
  const response = await api.post<{ rule: ProxyRule }>('/proxy/rules', data);
  return response.data.rule;
};

export const updateProxyRule = async (id: string, data: UpdateProxyRuleRequest): Promise<ProxyRule> => {
  const response = await api.put<{ rule: ProxyRule }>(`/proxy/rules/${id}`, data);
  return response.data.rule;
};

export const deleteProxyRule = async (id: string): Promise<void> => {
  await api.delete(`/proxy/rules/${id}`);
};

// ACME API
export const getAcmeStatus = async (): Promise<AcmeStatus> => {
  const response = await api.get<AcmeStatus>('/acme/status');
  return response.data;
};

export const getAcmeConfig = async (): Promise<AcmeConfig> => {
  const response = await api.get<AcmeConfig>('/acme/config');
  return response.data;
};

export const updateAcmeConfig = async (data: UpdateAcmeConfigRequest): Promise<AcmeConfig> => {
  const response = await api.put<AcmeConfig>('/acme/config', data);
  return response.data;
};

export const listAcmeCertificates = async (): Promise<AcmeCertificate[]> => {
  const response = await api.get<{ certificates: AcmeCertificate[] }>('/acme/certificates');
  return response.data.certificates;
};

export const requestAcmeCertificate = async (domain: string, challengeType: string = 'http-01'): Promise<AcmeCertificate> => {
  const response = await api.post<{ certificate: AcmeCertificate }>(`/acme/certificates/${domain}`, { challenge_type: challengeType });
  return response.data.certificate;
};

export const renewAcmeCertificate = async (domain: string): Promise<AcmeCertificate> => {
  const response = await api.post<{ certificate: AcmeCertificate }>(`/acme/certificates/${domain}/renew`);
  return response.data.certificate;
};

export const deleteAcmeCertificate = async (domain: string): Promise<void> => {
  await api.delete(`/acme/certificates/${domain}`);
};

// ACME DNS Provider API
export const getDnsProviders = async (): Promise<{ providers: string[]; config: DnsProviderConfig | null }> => {
  const response = await api.get('/acme/dns-providers');
  return response.data;
};

export const updateDnsProvider = async (config: DnsProviderConfig): Promise<{ success: boolean }> => {
  const response = await api.put('/acme/dns-providers', config);
  return response.data;
};

// ACME Challenge Status API
export const getChallengeStatus = async (domain: string): Promise<ChallengeStatus> => {
  const response = await api.get(`/acme/challenge-status/${domain}`);
  return response.data;
};

// Settings API
export const getSettings = async (): Promise<GeneralSettings> => {
  const response = await api.get<GeneralSettings>('/settings');
  return response.data;
};

export const getReverseProxyConfig = async (): Promise<ReverseProxySettings> => {
  const response = await api.get<ReverseProxySettings>('/settings/reverse-proxy');
  return response.data;
};

export const updateReverseProxyConfig = async (config: ReverseProxySettings): Promise<void> => {
  await api.put('/settings/reverse-proxy', config);
};

export const getDnsConfig = async (): Promise<DnsSettings> => {
  const response = await api.get<DnsSettings>('/settings/dns');
  return response.data;
};

export const updateDnsConfig = async (config: DnsSettings): Promise<void> => {
  await api.put('/settings/dns', config);
};

// ── LLM Gateway ──────────────────────────────────────────────

export async function getLlmGatewayConfig(): Promise<LlmGatewayConfig> {
  const { data } = await api.get('/llm/gateway');
  return data;
}

export async function updateLlmGatewayConfig(config: Partial<LlmGatewayConfig>): Promise<void> {
  await api.put('/llm/gateway', config);
}

// ── Providers ────────────────────────────────────────────────

export async function listLlmProviders(): Promise<LlmProvider[]> {
  const { data } = await api.get('/llm/providers');
  return data.providers;
}

export async function createLlmProvider(req: CreateProviderRequest): Promise<{ id: string }> {
  const { data } = await api.post('/llm/providers', req);
  return data;
}

export async function updateLlmProvider(id: string, req: CreateProviderRequest): Promise<void> {
  await api.put(`/llm/providers/${id}`, req);
}

export async function toggleLlmProvider(id: string, enabled: boolean): Promise<void> {
  await api.patch(`/llm/providers/${id}`, { enabled });
}

export async function deleteLlmProvider(id: string): Promise<void> {
  await api.delete(`/llm/providers/${id}`);
}

// ── Models ───────────────────────────────────────────────────

export async function listProviderModels(providerId: string): Promise<LlmModel[]> {
  const { data } = await api.get(`/llm/providers/${providerId}/models`);
  return data.models;
}

export async function listAllLlmModels(): Promise<LlmModel[]> {
  const { data } = await api.get('/llm/models');
  return data.models;
}

export async function addModel(providerId: string, req: CreateModelRequest): Promise<{ id: string }> {
  const { data } = await api.post(`/llm/providers/${providerId}/models`, req);
  return data;
}

export async function updateModel(id: string, req: CreateModelRequest): Promise<void> {
  await api.put(`/llm/models/${id}`, req);
}

export async function deleteModel(id: string): Promise<void> {
  await api.delete(`/llm/models/${id}`);
}

// ── API Keys ─────────────────────────────────────────────────

export async function listLlmApiKeys(): Promise<LlmApiKey[]> {
  const { data } = await api.get('/llm/api-keys');
  return data.api_keys;
}

export async function createLlmApiKey(name: string): Promise<CreateApiKeyResponse> {
  const { data } = await api.post('/llm/api-keys', { name });
  return data;
}

export async function toggleLlmApiKey(id: string, enabled: boolean): Promise<void> {
  await api.patch(`/llm/api-keys/${id}`, { enabled });
}

/** 绑定/解绑 API 密钥的 RAG 知识库。kbId 为 null 时解绑。 */
export async function bindLlmApiKey(id: string, kbId: string | null): Promise<void> {
  await api.patch(`/llm/api-keys/${id}`, { kb_id: kbId });
}

export async function deleteLlmApiKey(id: string): Promise<void> {
  await api.delete(`/llm/api-keys/${id}`);
}

// ── Usage stats ──────────────────────────────────────────────

interface UsageRangeParams {
  start?: string;
  end?: string;
}

export async function getLlmUsageSummary(params: UsageRangeParams): Promise<LlmUsageSummary> {
  const { data } = await api.get('/llm/usage/summary', { params });
  return data.summary;
}

export async function getLlmUsageAggregate(
  groupBy: UsageGroupBy,
  params: UsageRangeParams
): Promise<LlmUsageAggregateRow[]> {
  const { data } = await api.get('/llm/usage/aggregate', {
    params: { ...params, group_by: groupBy },
  });
  return data.rows;
}

export async function getLlmUsageLogs(
  params: UsageRangeParams & { limit?: number; offset?: number }
): Promise<LlmUsageLogsResponse> {
  const { data } = await api.get('/llm/usage/logs', { params });
  return { logs: data.logs, total: data.total };
}

// ── RAG Knowledge Base ──────────────────────────────────────────

export async function listLlmKbs(): Promise<LlmKnowledgeBase[]> {
  const { data } = await api.get('/llm/kb');
  return data.knowledge_bases;
}

export async function getLlmKb(id: string): Promise<LlmKnowledgeBase> {
  const { data } = await api.get(`/llm/kb/${encodeURIComponent(id)}`);
  return data;
}

export async function createLlmKb(req: CreateLlmKbRequest): Promise<{ id: string }> {
  const { data } = await api.post('/llm/kb', req);
  return data;
}

export async function updateLlmKb(id: string, req: UpdateLlmKbRequest): Promise<void> {
  await api.put(`/llm/kb/${encodeURIComponent(id)}`, req);
}

export async function toggleLlmKb(id: string, enabled: boolean): Promise<void> {
  await api.patch(`/llm/kb/${encodeURIComponent(id)}`, { enabled });
}

export async function deleteLlmKb(id: string): Promise<void> {
  await api.delete(`/llm/kb/${encodeURIComponent(id)}`);
}

export async function listLlmKbDocs(kbId: string): Promise<LlmKbDocument[]> {
  const { data } = await api.get(`/llm/kb/${encodeURIComponent(kbId)}/docs`);
  return data.documents;
}

export async function uploadKbDoc(kbId: string, file: File): Promise<LlmKbDocument> {
  const formData = new FormData();
  formData.append('file', file);
  const { data } = await api.post(`/llm/kb/${encodeURIComponent(kbId)}/docs`, formData);
  return data;
}

export async function deleteKbDoc(kbId: string, docId: string): Promise<void> {
  await api.delete(`/llm/kb/${encodeURIComponent(kbId)}/docs/${encodeURIComponent(docId)}`);
}

export async function reindexKbDoc(kbId: string, docId: string): Promise<LlmKbDocument> {
  const { data } = await api.post(`/llm/kb/${encodeURIComponent(kbId)}/docs/${encodeURIComponent(docId)}/reindex`);
  return data;
}

export async function testEmbedding(req: {
  base_url: string;
  api_key: string;
  model: string;
}): Promise<TestEmbeddingResult> {
  const { data } = await api.post('/llm/kb/test-embedding', req);
  return data;
}

export async function queryKb(kbId: string, text: string): Promise<KbQueryResult> {
  const { data } = await api.post(`/llm/kb/${encodeURIComponent(kbId)}/query`, { text });
  return data;
}

/**
 * Extract a human-readable message from an API error.
 * Axum handlers on this server reply with plain-text bodies on failure
 * (e.g. `(StatusCode::BAD_REQUEST, "name is required")`), but some endpoints
 * return JSON `{ "error": "..." }`. Falls back to the generic Error message.
 */
export function getApiErrorMessage(err: unknown): string {
  const data = (err as { response?: { data?: unknown } } | null)?.response?.data;
  if (typeof data === 'string' && data.trim() !== '') {
    return data;
  }
  if (data && typeof data === 'object') {
    const msg = (data as { error?: unknown }).error;
    if (typeof msg === 'string' && msg.trim() !== '') {
      return msg;
    }
  }
  return err instanceof Error ? err.message : String(err);
}


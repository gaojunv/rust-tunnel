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
  LlmModelGroup,
  LlmModelGroupDetail,
  AgentWorkspace,
  AgentSession,
  AgentMessage,
  AgentMemory,
  AgentMemorySettings,
  AgentMemoriesResponse,
  CreateMemoryRequest,
  UpdateMemoryRequest,
  MemorySettingsRequest,
  AgentSkill,
  AgentSkillsResponse,
  CreateSkillRequest,
  UpdateSkillRequest,
  AgentRole,
  AgentRolesResponse,
  RoleListParams,
  CreateRoleRequest,
  UpdateRoleRequest,
  FsEntry,
  FsFileContent,
  GitStatusResult,
  GitBranch,
  GitCommit,
  GitStashEntry,
  GitBranchesResult,
  GitLogResult,
  GitStashesResult,
  GitDiffResult,
  GhRepoState,
  GhWorkflowList,
  GhWorkflowRuns,
  GhJobList,
  GhJobLogs,
  GhWriteResult,
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
//
// 后端已统一为 /api/knowledge（KB 向量索引与 Wiki 页面索引同容器双索引）。
// 本节与下方 Wiki 段共用该端点：本节固定 index_kind=vector / index_vector=true，
// Wiki 段固定 pages——旧调用方语义不变，批 5 前端整合时再收敛为一套。

/** 统一文档视图：per-kind 状态挂在 vector/pages 子对象上（未启用为 null）。 */
interface UnifiedDoc {
  id: string;
  source_id: string;
  filename: string;
  file_type: string;
  content_hash: string;
  created_at: string;
  updated_at: string;
  vector: { status: string; chunk_count: number; error?: string | null } | null;
  pages: { status: string; page_count: number; error?: string | null } | null;
}

const toKbDoc = (d: UnifiedDoc): LlmKbDocument => ({
  id: d.id,
  kb_id: d.source_id,
  filename: d.filename,
  file_type: d.file_type,
  content_hash: d.content_hash,
  status: (d.vector?.status ?? 'pending') as LlmKbDocument['status'],
  chunk_count: d.vector?.chunk_count ?? 0,
  error: d.vector?.error ?? null,
  created_at: d.created_at,
  updated_at: d.updated_at,
});

export async function listLlmKbs(): Promise<LlmKnowledgeBase[]> {
  const { data } = await api.get('/knowledge', { params: { index_kind: 'vector' } });
  return data.sources;
}

export async function getLlmKb(id: string): Promise<LlmKnowledgeBase> {
  const { data } = await api.get(`/knowledge/${encodeURIComponent(id)}`);
  return data;
}

export async function createLlmKb(req: CreateLlmKbRequest): Promise<{ id: string }> {
  const { data } = await api.post('/knowledge', { ...req, index_vector: true });
  return data;
}

export async function updateLlmKb(id: string, req: UpdateLlmKbRequest): Promise<void> {
  await api.put(`/knowledge/${encodeURIComponent(id)}`, req);
}

export async function toggleLlmKb(id: string, enabled: boolean): Promise<void> {
  await api.patch(`/knowledge/${encodeURIComponent(id)}`, { enabled });
}

export async function deleteLlmKb(id: string): Promise<void> {
  await api.delete(`/knowledge/${encodeURIComponent(id)}`);
}

export async function listLlmKbDocs(kbId: string): Promise<LlmKbDocument[]> {
  const { data } = await api.get(`/knowledge/${encodeURIComponent(kbId)}/docs`);
  return (data.documents as UnifiedDoc[]).map(toKbDoc);
}

export async function uploadKbDoc(kbId: string, file: File): Promise<LlmKbDocument> {
  const formData = new FormData();
  formData.append('file', file);
  const { data } = await api.post(`/knowledge/${encodeURIComponent(kbId)}/docs`, formData);
  return toKbDoc(data as UnifiedDoc);
}

export async function deleteKbDoc(kbId: string, docId: string): Promise<void> {
  await api.delete(`/knowledge/${encodeURIComponent(kbId)}/docs/${encodeURIComponent(docId)}`);
}

export async function reindexKbDoc(kbId: string, docId: string): Promise<LlmKbDocument> {
  const { data } = await api.post(`/knowledge/${encodeURIComponent(kbId)}/docs/${encodeURIComponent(docId)}/reindex`);
  return toKbDoc(data as UnifiedDoc);
}

export async function testEmbedding(req: {
  base_url: string;
  api_key: string;
  model: string;
  /** 编辑已有 KB 时传入：api_key 留空则用该 KB 已存密钥测试。 */
  kb_id?: string;
}): Promise<TestEmbeddingResult> {
  const { data } = await api.post('/knowledge/test-embedding', req);
  return data;
}

export async function queryKb(kbId: string, text: string): Promise<KbQueryResult> {
  const { data } = await api.post(`/knowledge/${encodeURIComponent(kbId)}/query`, { text });
  return data;
}

// ── LLM 模型组（多模型故障转移） ──────────────────────────────────

export async function listLlmModelGroups(): Promise<LlmModelGroup[]> {
  const { data } = await api.get('/llm/model-groups');
  return data.groups;
}

export async function createLlmModelGroup(req: {
  name: string;
  enabled?: boolean;
}): Promise<{ id: string }> {
  const { data } = await api.post('/llm/model-groups', req);
  return data;
}

export async function getLlmModelGroup(id: string): Promise<LlmModelGroupDetail> {
  const { data } = await api.get(`/llm/model-groups/${id}`);
  return data;
}

export async function updateLlmModelGroup(
  id: string,
  req: { name: string; enabled?: boolean },
): Promise<void> {
  await api.put(`/llm/model-groups/${id}`, req);
}

export async function deleteLlmModelGroup(id: string): Promise<void> {
  await api.delete(`/llm/model-groups/${id}`);
}

export async function replaceGroupMembers(
  id: string,
  members: { model_id: string; priority: number }[],
): Promise<void> {
  await api.put(`/llm/model-groups/${id}/members`, { members });
}

export async function resetGroupBreaker(id: string): Promise<{ reset: number }> {
  const { data } = await api.post(`/llm/model-groups/${id}/reset-breaker`, {});
  return data;
}

// ── Agent Workbench ───────────────────────────────────────────

export async function listAgentWorkspaces(): Promise<AgentWorkspace[]> {
  const { data } = await api.get('/agent/workspaces');
  return data;
}

export async function createAgentWorkspace(body: {
  name: string;
  client_id: string;
  runtime_type: string;
  root_path: string;
  docker_image?: string;
  docker_container_id?: string;
  agent_type?: string;
  agent_path?: string;
  llm_model_id?: string;
  agent_config_overrides?: string | null;
  /** Claude Code 三档位模型映射（JSON object：{opus,sonnet,haiku} → `model:<id>`/`group:<id>`） */
  claude_tier_models?: string | null;
  /** GitHub Actions 面板：owner/repo 空串=不设置；token 仅在非空时发送（服务端加密落库） */
  github_owner?: string;
  github_repo?: string;
  github_token?: string;
}): Promise<AgentWorkspace> {
  const { data } = await api.post('/agent/workspaces', body);
  return data;
}

export async function deleteAgentWorkspace(id: string): Promise<void> {
  await api.delete(`/agent/workspaces/${id}`);
}

export const updateAgentWorkspace = (
  id: string,
  body: {
    name: string;
    root_path: string;
    system_prompt?: string;
    approval_mode?: string;
    agent_type?: string;
    agent_path?: string;
    llm_model_id?: string;
    agent_config_overrides?: string | null;
    /** Claude Code 三档位模型映射（JSON object；显式 null 清空，缺省保持原值） */
    claude_tier_models?: string | null;
    /** GitHub 字段：空串=保持不变（服务端 COALESCE）；token 仅在非空时发送 */
    github_owner?: string;
    github_repo?: string;
    github_token?: string;
  },
) => api.put(`/agent/workspaces/${id}`, body);

export const listWorkspaceFiles = (workspaceId: string, q: string) =>
  api.get<{ files: string[] }>(`/agent/workspaces/${workspaceId}/files`, { params: { q, limit: 20 } })
    .then((r) => r.data);

export async function listAgentSessions(workspaceId: string): Promise<AgentSession[]> {
  const { data } = await api.get(`/agent/workspaces/${workspaceId}/sessions`);
  return data;
}

export async function createAgentSession(
  workspaceId: string,
  title?: string,
  model?: string
): Promise<AgentSession> {
  const { data } = await api.post(`/agent/workspaces/${workspaceId}/sessions`, { title, model });
  return data;
}

export async function deleteAgentSession(id: string): Promise<void> {
  await api.delete(`/agent/sessions/${id}`);
}

export async function updateAgentSessionTitle(id: string, title: string): Promise<void> {
  await api.put(`/agent/sessions/${id}`, { title });
}

/** 导出会话为 Markdown：返回 Blob（调用方负责触发下载与 revokeObjectURL）。 */
export async function exportAgentSession(id: string): Promise<Blob> {
  const { data } = await api.get(`/agent/sessions/${id}/export`, {
    responseType: 'blob',
  });
  return data as Blob;
}

/** 会话消息分页响应：`messages` 为升序的最近一页，`has_more` 表示是否还有更早。 */
export interface AgentMessagesPage {
  messages: AgentMessage[];
  has_more: boolean;
}

export async function listAgentMessages(
  sessionId: string,
  opts?: { before?: string; limit?: number },
): Promise<AgentMessagesPage> {
  const { data } = await api.get<AgentMessagesPage>(
    `/agent/sessions/${sessionId}/messages`,
    { params: opts },
  );
  return data;
}

export async function updateAgentSessionModel(id: string, model: string): Promise<void> {
  await api.patch(`/agent/sessions/${id}/model`, { model });
}

export async function getAgentDefaultModel(): Promise<string> {
  const { data } = await api.get('/agent/default-model');
  return data.model ?? '';
}

export async function putAgentDefaultModel(model: string): Promise<void> {
  await api.put('/agent/default-model', { model });
}

// ── Agent Memory ────────────────────────────────────────────────

export async function getMemorySettings(): Promise<AgentMemorySettings> {
  const { data } = await api.get('/agent/memory/settings');
  return data;
}

export async function updateMemorySettings(req: MemorySettingsRequest): Promise<AgentMemorySettings> {
  const { data } = await api.put('/agent/memory/settings', req);
  return data;
}

export async function testMemoryEmbedding(req: {
  base_url: string;
  api_key: string;
  model: string;
}): Promise<TestEmbeddingResult> {
  const { data } = await api.post('/agent/memory/settings/test-embedding', req);
  return data;
}

export async function clearMemory(): Promise<void> {
  await api.post('/agent/memory/clear');
}

/** 记忆列表查询参数；空值会自动剔除（不进 URL）。 */
export interface MemoryListParams {
  scope?: string;
  client_id?: string;
  workspace_id?: string;
  q?: string;
  pinned?: boolean;
  sort?: string;
  limit?: number;
  offset?: number;
}

export async function listMemories(params: MemoryListParams = {}): Promise<AgentMemoriesResponse> {
  const clean: Record<string, string | number> = {};
  if (params.scope) clean.scope = params.scope;
  if (params.client_id) clean.client_id = params.client_id;
  if (params.workspace_id) clean.workspace_id = params.workspace_id;
  if (params.q) clean.q = params.q;
  if (params.pinned) clean.pinned = 'true';
  if (params.sort) clean.sort = params.sort;
  if (params.limit !== undefined) clean.limit = params.limit;
  if (params.offset !== undefined) clean.offset = params.offset;
  const { data } = await api.get('/agent/memories', { params: clean });
  return { memories: data.memories ?? [], total: data.total ?? 0 };
}

export async function getMemory(id: string): Promise<AgentMemory> {
  const { data } = await api.get(`/agent/memories/${encodeURIComponent(id)}`);
  return data;
}

export async function createMemory(req: CreateMemoryRequest): Promise<AgentMemory> {
  const { data } = await api.post('/agent/memories', req);
  return data;
}

export async function updateMemory(id: string, req: UpdateMemoryRequest): Promise<AgentMemory> {
  const { data } = await api.put(`/agent/memories/${encodeURIComponent(id)}`, req);
  return data;
}

export async function deleteMemory(id: string): Promise<void> {
  await api.delete(`/agent/memories/${encodeURIComponent(id)}`);
}

export async function pinMemory(id: string): Promise<AgentMemory> {
  const { data } = await api.post(`/agent/memories/${encodeURIComponent(id)}/pin`);
  return data;
}

/** 手动重蒸馏指定会话（复位 distilled=0 重跑）。 */
export async function distillSession(sessionId: string): Promise<void> {
  await api.post(`/agent/sessions/${encodeURIComponent(sessionId)}/distill`);
}

// ── Agent Skill ──────────────────────────────────────────────────

/** 技能列表查询参数；空值会自动剔除（不进 URL）。 */
export interface SkillListParams {
  scope?: string;
  client_id?: string;
  workspace_id?: string;
  q?: string;
  enabled?: boolean;
  sort?: string;
  limit?: number;
  offset?: number;
}

export async function listSkills(params: SkillListParams = {}): Promise<AgentSkillsResponse> {
  const clean: Record<string, string | number> = {};
  if (params.scope) clean.scope = params.scope;
  if (params.client_id) clean.client_id = params.client_id;
  if (params.workspace_id) clean.workspace_id = params.workspace_id;
  if (params.q) clean.q = params.q;
  if (params.enabled !== undefined) clean.enabled = params.enabled ? 'true' : 'false';
  if (params.sort) clean.sort = params.sort;
  if (params.limit !== undefined) clean.limit = params.limit;
  if (params.offset !== undefined) clean.offset = params.offset;
  const { data } = await api.get('/agent/skills', { params: clean });
  return { skills: data.skills ?? [], total: data.total ?? 0 };
}

export async function getSkill(id: string): Promise<AgentSkill> {
  const { data } = await api.get(`/agent/skills/${encodeURIComponent(id)}`);
  return data;
}

export async function createSkill(req: CreateSkillRequest): Promise<AgentSkill> {
  const { data } = await api.post('/agent/skills', req);
  return data;
}

export async function updateSkill(id: string, req: UpdateSkillRequest): Promise<AgentSkill> {
  const { data } = await api.put(`/agent/skills/${encodeURIComponent(id)}`, req);
  return data;
}

export async function deleteSkill(id: string): Promise<void> {
  await api.delete(`/agent/skills/${encodeURIComponent(id)}`);
}

export async function toggleSkill(id: string): Promise<AgentSkill> {
  const { data } = await api.post(`/agent/skills/${encodeURIComponent(id)}/toggle`);
  return data;
}

// ── Wiki（批 4 完整） ──────────────────────────────────────────

export interface WikiListParams {
  scope?: string;
  client_id?: string;
  workspace_id?: string;
  q?: string;
  status?: string;
  limit?: number;
  offset?: number;
}

export async function listWikis(params: WikiListParams = {}): Promise<import('../types').AgentWikisResponse> {
  const clean: Record<string, string | number> = { index_kind: 'pages' };
  if (params.scope) clean.scope = params.scope;
  if (params.client_id) clean.client_id = params.client_id;
  if (params.workspace_id) clean.workspace_id = params.workspace_id;
  if (params.q) clean.q = params.q;
  if (params.status) clean.status = params.status;
  if (params.limit !== undefined) clean.limit = params.limit;
  if (params.offset !== undefined) clean.offset = params.offset;
  const { data } = await api.get('/knowledge', { params: clean });
  return { wikis: data.sources ?? [], total: data.total ?? 0 };
}

export async function createWiki(req: import('../types').CreateWikiRequest): Promise<import('../types').AgentWiki> {
  const { data } = await api.post('/knowledge', { ...req, index_pages: true });
  return data;
}

export async function getWiki(id: string): Promise<import('../types').AgentWiki> {
  const { data } = await api.get(`/knowledge/${encodeURIComponent(id)}`);
  return data;
}

export async function updateWiki(id: string, req: import('../types').UpdateWikiRequest): Promise<import('../types').AgentWiki> {
  const { data } = await api.patch(`/knowledge/${encodeURIComponent(id)}`, req);
  return data;
}

export async function deleteWiki(id: string): Promise<void> {
  await api.delete(`/knowledge/${encodeURIComponent(id)}`);
}

// ── Wiki 文档 ──────────────────────────────────────────────────

const toWikiDoc = (d: UnifiedDoc): import('../types').WikiDocument => ({
  id: d.id,
  wiki_id: d.source_id,
  filename: d.filename,
  file_type: d.file_type,
  content_hash: d.content_hash,
  status: (d.pages?.status ?? 'pending') as import('../types').WikiDocument['status'],
  error: d.pages?.error ?? null,
  created_at: d.created_at,
  updated_at: d.updated_at,
});

export async function listWikiDocs(wikiId: string): Promise<import('../types').WikiDocsResponse> {
  const { data } = await api.get(`/knowledge/${encodeURIComponent(wikiId)}/docs`);
  return { documents: ((data.documents ?? []) as UnifiedDoc[]).map(toWikiDoc) };
}

export async function uploadWikiDoc(wikiId: string, file: File): Promise<import('../types').WikiDocument> {
  const formData = new FormData();
  formData.append('file', file);
  const { data } = await api.post(`/knowledge/${encodeURIComponent(wikiId)}/docs`, formData);
  return toWikiDoc(data as UnifiedDoc);
}

export async function deleteWikiDoc(wikiId: string, docId: string): Promise<void> {
  await api.delete(`/knowledge/${encodeURIComponent(wikiId)}/docs/${encodeURIComponent(docId)}`);
}

export async function reindexWikiDoc(wikiId: string, docId: string): Promise<{ status: string; id: string }> {
  const { data } = await api.post(`/knowledge/${encodeURIComponent(wikiId)}/docs/${encodeURIComponent(docId)}/reindex`);
  return data;
}

// ── Wiki 页面（ref 含 `/`，整段 encodeURIComponent 交给后端 wildcard 路由） ──

export async function listWikiPages(
  wikiId: string,
  params: import('../types').WikiPageListParams = {},
): Promise<import('../types').WikiPagesResponse> {
  const clean: Record<string, string | number | boolean> = {};
  if (params.q) clean.q = params.q;
  if (params.ref_prefix) clean.ref_prefix = params.ref_prefix;
  if (params.locked !== undefined) clean.locked = params.locked ? 'true' : 'false';
  if (params.limit !== undefined) clean.limit = params.limit;
  if (params.offset !== undefined) clean.offset = params.offset;
  const { data } = await api.get(`/knowledge/${encodeURIComponent(wikiId)}/pages`, { params: clean });
  return { pages: data.pages ?? [], total: data.total ?? 0 };
}

export async function getWikiPage(wikiId: string, ref: string): Promise<import('../types').WikiPage> {
  const { data } = await api.get(`/knowledge/${encodeURIComponent(wikiId)}/pages/${encodeURIComponent(ref)}`);
  return data;
}

export async function putWikiPage(
  wikiId: string,
  ref: string,
  req: import('../types').PutWikiPageRequest,
): Promise<import('../types').WikiPage> {
  const { data } = await api.put(`/knowledge/${encodeURIComponent(wikiId)}/pages/${encodeURIComponent(ref)}`, req);
  return data;
}

export async function deleteWikiPage(wikiId: string, ref: string): Promise<void> {
  await api.delete(`/knowledge/${encodeURIComponent(wikiId)}/pages/${encodeURIComponent(ref)}`);
}

// ── Wiki 搜索 / 图谱 ───────────────────────────────────────────

export async function searchWiki(
  wikiId: string,
  q: string,
  limit = 20,
): Promise<import('../types').WikiSearchResponse> {
  const { data } = await api.get(`/knowledge/${encodeURIComponent(wikiId)}/search`, { params: { q, limit } });
  return { hits: data.hits ?? [] };
}

export async function searchAllWikis(q: string, limit = 20): Promise<import('../types').WikiSearchResponse> {
  const { data } = await api.get('/knowledge/search', { params: { q, limit } });
  return { hits: data.hits ?? [] };
}

export async function getWikiGraph(wikiId: string): Promise<import('../types').WikiGraphData> {
  const { data } = await api.get(`/knowledge/${encodeURIComponent(wikiId)}/graph`);
  return { nodes: data.nodes ?? [], edges: data.edges ?? [] };
}

// ── Agent Role（多角色子代理系统） ────────────────────────────────

export async function listRoles(params: RoleListParams = {}): Promise<AgentRolesResponse> {
  const clean: Record<string, string | number> = {};
  if (params.scope) clean.scope = params.scope;
  if (params.client_id) clean.client_id = params.client_id;
  if (params.workspace_id) clean.workspace_id = params.workspace_id;
  if (params.q) clean.q = params.q;
  if (params.enabled !== undefined) clean.enabled = params.enabled ? 'true' : 'false';
  const { data } = await api.get('/agent/roles', { params: clean });
  return { roles: data.roles ?? [], total: data.total ?? 0 };
}

export async function getRole(id: string): Promise<AgentRole> {
  const { data } = await api.get(`/agent/roles/${encodeURIComponent(id)}`);
  return data;
}

export async function createRole(req: CreateRoleRequest): Promise<AgentRole> {
  const { data } = await api.post('/agent/roles', req);
  return data;
}

export async function updateRole(id: string, req: UpdateRoleRequest): Promise<AgentRole> {
  const { data } = await api.put(`/agent/roles/${encodeURIComponent(id)}`, req);
  return data;
}

export async function deleteRole(id: string): Promise<void> {
  await api.delete(`/agent/roles/${encodeURIComponent(id)}`);
}

export async function toggleRole(id: string): Promise<AgentRole> {
  const { data } = await api.patch(`/agent/roles/${encodeURIComponent(id)}/toggle`);
  return data;
}

export async function updateAgentSessionRole(
  sessionId: string,
  roleId: string | null,
): Promise<void> {
  await api.patch(`/agent/sessions/${encodeURIComponent(sessionId)}/role`, { role_id: roleId });
}

export function agentWsUrl(sessionId: string): string {
  const token = localStorage.getItem('auth_token') ?? '';
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${proto}//${location.host}/api/agent/ws?session_id=${sessionId}&token=${encodeURIComponent(token)}`;
}

/** 工作台全局通知 WS（标签闪动/系统通知用）。应用级建立一条，订阅所有会话事件。 */
export function agentNotificationsWsUrl(): string {
  const token = localStorage.getItem('auth_token') ?? '';
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${proto}//${location.host}/api/agent/notifications/ws?token=${encodeURIComponent(token)}`;
}

export async function getFsTree(workspaceId: string, path?: string): Promise<FsEntry[]> {
  const { data } = await api.get<{ entries: FsEntry[] }>(
    `/agent/workspaces/${workspaceId}/fs/tree`,
    { params: path ? { path } : undefined },
  );
  return data.entries;
}

export async function getFsFile(workspaceId: string, path: string): Promise<FsFileContent> {
  const { data } = await api.get<FsFileContent>(
    `/agent/workspaces/${workspaceId}/fs/file`,
    { params: { path } },
  );
  return data;
}

export async function putFsFile(
  workspaceId: string,
  path: string,
  content: string,
  approved?: boolean,
): Promise<void> {
  await api.put(`/agent/workspaces/${workspaceId}/fs/file`, {
    path,
    content,
    ...(approved !== undefined ? { approved } : {}),
  });
}

export async function getAgentGitStatus(workspaceId: string): Promise<GitStatusResult> {
  const { data } = await api.get<GitStatusResult>(
    `/agent/workspaces/${workspaceId}/git/status`,
  );
  return data;
}

export async function getAgentGitDiff(
  workspaceId: string,
  path?: string,
  cached?: boolean,
): Promise<string> {
  const { data } = await api.get<{ diff: string }>(
    `/agent/workspaces/${workspaceId}/git/diff`,
    { params: { ...(path ? { path } : {}), ...(cached ? { cached: true } : {}) } },
  );
  return data.diff;
}

export async function getAgentGitBranches(workspaceId: string): Promise<GitBranch[]> {
  const { data } = await api.get<GitBranchesResult>(
    `/agent/workspaces/${workspaceId}/git/branches`,
  );
  return data.branches;
}

export async function getAgentGitLog(workspaceId: string, limit = 50): Promise<GitCommit[]> {
  const { data } = await api.get<GitLogResult>(
    `/agent/workspaces/${workspaceId}/git/log`,
    { params: { limit } },
  );
  return data.commits;
}

export async function getAgentGitShow(workspaceId: string, rev: string): Promise<string> {
  const { data } = await api.get<GitDiffResult>(
    `/agent/workspaces/${workspaceId}/git/show`,
    { params: { rev } },
  );
  return data.diff;
}

export async function getAgentGitStashes(workspaceId: string): Promise<GitStashEntry[]> {
  const { data } = await api.get<GitStashesResult>(
    `/agent/workspaces/${workspaceId}/git/stash`,
  );
  return data.stashes;
}

/** git 写操作统一封装：body 为请求体（不含 approved），approved 为审批确认标记。 */
async function postAgentGit(
  workspaceId: string,
  path: string,
  body: Record<string, unknown>,
  approved?: boolean,
): Promise<void> {
  await api.post(`/agent/workspaces/${workspaceId}/git/${path}`, {
    ...body,
    ...(approved !== undefined ? { approved } : {}),
  });
}

export function postAgentGitStage(
  workspaceId: string,
  paths: string[],
  approved?: boolean,
): Promise<void> {
  return postAgentGit(workspaceId, 'stage', { paths }, approved);
}

export function postAgentGitUnstage(
  workspaceId: string,
  paths: string[],
  approved?: boolean,
): Promise<void> {
  return postAgentGit(workspaceId, 'unstage', { paths }, approved);
}

export function postAgentGitCommit(
  workspaceId: string,
  message: string,
  approved?: boolean,
): Promise<void> {
  return postAgentGit(workspaceId, 'commit', { message }, approved);
}

export function postAgentGitCheckout(
  workspaceId: string,
  branch: string,
  create?: boolean,
  approved?: boolean,
): Promise<void> {
  return postAgentGit(
    workspaceId,
    'checkout',
    { branch, ...(create ? { create: true } : {}) },
    approved,
  );
}

export function postAgentGitBranchDelete(
  workspaceId: string,
  branch: string,
  force?: boolean,
  approved?: boolean,
): Promise<void> {
  return postAgentGit(
    workspaceId,
    'branch/delete',
    { branch, ...(force ? { force: true } : {}) },
    approved,
  );
}

export function postAgentGitPull(workspaceId: string, approved?: boolean): Promise<void> {
  return postAgentGit(workspaceId, 'pull', {}, approved);
}

export function postAgentGitPush(workspaceId: string, approved?: boolean): Promise<void> {
  return postAgentGit(workspaceId, 'push', {}, approved);
}

export function postAgentGitRevert(
  workspaceId: string,
  rev: string,
  approved?: boolean,
): Promise<void> {
  return postAgentGit(workspaceId, 'revert', { rev }, approved);
}

export function postAgentGitReset(
  workspaceId: string,
  mode: 'soft' | 'mixed' | 'hard',
  rev?: string,
  approved?: boolean,
): Promise<void> {
  return postAgentGit(
    workspaceId,
    'reset',
    { mode, ...(rev ? { rev } : {}) },
    approved,
  );
}

export function postAgentGitStashPush(
  workspaceId: string,
  message?: string,
  approved?: boolean,
): Promise<void> {
  return postAgentGit(
    workspaceId,
    'stash',
    { ...(message ? { message } : {}) },
    approved,
  );
}

export function postAgentGitStashApply(
  workspaceId: string,
  index: number,
  approved?: boolean,
): Promise<void> {
  return postAgentGit(workspaceId, 'stash/apply', { index }, approved);
}

export function postAgentGitStashPop(
  workspaceId: string,
  index: number,
  approved?: boolean,
): Promise<void> {
  return postAgentGit(workspaceId, 'stash/pop', { index }, approved);
}

export function postAgentGitStashDrop(
  workspaceId: string,
  index: number,
  approved?: boolean,
): Promise<void> {
  return postAgentGit(workspaceId, 'stash/drop', { index }, approved);
}

// ── GitHub Actions 面板（AI 工作台）────────────────────────────

/** GET /github/repo — 仓库定位检测。`refresh=true` 强制经隧道重探 remote。 */
export async function getAgentGithubRepo(
  workspaceId: string,
  refresh?: boolean,
): Promise<GhRepoState> {
  const { data } = await api.get<GhRepoState>(
    `/agent/workspaces/${workspaceId}/github/repo`,
    { params: { ...(refresh ? { refresh: true } : {}) } },
  );
  return data;
}

/** GET /github/workflows — GitHub 原生工作流列表。 */
export async function getAgentGithubWorkflows(
  workspaceId: string,
): Promise<GhWorkflowList> {
  const { data } = await api.get<GhWorkflowList>(
    `/agent/workspaces/${workspaceId}/github/workflows`,
  );
  return data;
}

/** GET /github/runs — 工作流运行列表（GitHub 原生响应）。 */
export async function getAgentGithubRuns(
  workspaceId: string,
  opts?: { workflow_id?: string; per_page?: number },
): Promise<GhWorkflowRuns> {
  const { data } = await api.get<GhWorkflowRuns>(
    `/agent/workspaces/${workspaceId}/github/runs`,
    { params: opts },
  );
  return data;
}

/** GET /github/runs/:run_id/jobs — 某次运行的作业列表。 */
export async function getAgentGithubRunJobs(
  workspaceId: string,
  runId: string,
): Promise<GhJobList> {
  const { data } = await api.get<GhJobList>(
    `/agent/workspaces/${workspaceId}/github/runs/${runId}/jobs`,
  );
  return data;
}

/** GET /github/jobs/:job_id/logs — 作业日志（尾部 64KB）。 */
export async function getAgentGithubJobLogs(
  workspaceId: string,
  jobId: string,
): Promise<GhJobLogs> {
  const { data } = await api.get<GhJobLogs>(
    `/agent/workspaces/${workspaceId}/github/jobs/${jobId}/logs`,
  );
  return data;
}

/** GitHub 写操作统一封装：body 不含 approved（首次无确认直接 409 审批拦截）。 */
async function postAgentGithub(
  workspaceId: string,
  path: string,
  body: Record<string, unknown>,
  approved?: boolean,
): Promise<GhWriteResult> {
  const { data } = await api.post<GhWriteResult>(
    `/agent/workspaces/${workspaceId}/github/${path}`,
    { ...body, ...(approved !== undefined ? { approved } : {}) },
  );
  return data;
}

/** POST workflow_dispatch：ref 必填，inputs 可选。 */
export function postAgentGithubDispatch(
  workspaceId: string,
  workflowId: string,
  ref: string,
  inputs?: Record<string, string>,
  approved?: boolean,
): Promise<GhWriteResult> {
  return postAgentGithub(
    workspaceId,
    `workflows/${workflowId}/dispatch`,
    { ref, ...(inputs && Object.keys(inputs).length > 0 ? { inputs } : {}) },
    approved,
  );
}

/** POST rerun：重跑某次运行。 */
export function postAgentGithubRerun(
  workspaceId: string,
  runId: string,
  approved?: boolean,
): Promise<GhWriteResult> {
  return postAgentGithub(workspaceId, `runs/${runId}/rerun`, {}, approved);
}

/** POST cancel：取消进行中的运行。 */
export function postAgentGithubCancel(
  workspaceId: string,
  runId: string,
  approved?: boolean,
): Promise<GhWriteResult> {
  return postAgentGithub(workspaceId, `runs/${runId}/cancel`, {}, approved);
}

export function agentTerminalWsUrl(workspaceId: string, cols: number, rows: number): string {
  const token = localStorage.getItem('auth_token') ?? '';
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${proto}//${location.host}/api/agent/terminal/ws?workspace_id=${workspaceId}&cols=${cols}&rows=${rows}&token=${encodeURIComponent(token)}`;
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


import type { PlanEntryItem, ToolDiff, ToolKind, ToolLocation } from '../components/agent/types';

export interface LoginRequest {
  password: string;
}

export interface LoginResponse {
  token: string;
  auth_required: boolean;
}

// Shadowsocks configuration
export interface ShadowsocksConfig {
  enabled: boolean;
  port?: number;
  cipher?: string;
}

// Trojan configuration
export interface TrojanConfig {
  enabled: boolean;
  port?: number;
  fallback?: string;
  domain?: string;
  cert_source?: 'acme_exact' | 'acme_wildcard' | 'self_signed';
  shared?: boolean;
}

// Log entry
export interface LogEntry {
  id: number;
  timestamp: number;       // microsecond timestamp
  level: string;           // TRACE/DEBUG/INFO/WARN/ERROR
  source: string;          // "server" or "client:{hostname}:{port}"
  target: string;          // tracing target (module path)
  message: string;         // log message content
}

// Mesh network types
export interface MeshMemberResponse {
  client_name: string;
  public_addr?: string;
  p2p_available: boolean;
  online: boolean;
}

export interface MeshServiceResponse {
  service_name: string;
  protocol: string;
  local_addr: string;
  client_name: string;
}

export interface MeshNetworkResponse {
  id: string;
  members: MeshMemberResponse[];
  services: MeshServiceResponse[];
}

// DNS types
export interface DnsRecordResponse {
  name: string;
  record_type: string;
  value: string;
}

export interface AddDnsRecordRequest {
  name: string;
  record_type: string;
  value: string;
  port?: number;
}

// === Reverse Proxy ===

export type RuleType = 'http' | 'tcp' | 'udp';
export type LoadBalancing = 'round_robin' | 'weighted_round_robin';
export type BackendKind = 'direct' | 'client';
export type BackendScheme = 'http' | 'https';
export type BackendProtocol = 'http1' | 'http2';

export interface Backend {
  kind: BackendKind;
  addr: string;
  client_name?: string | null;
  weight: number;
  scheme?: BackendScheme;
  protocol?: BackendProtocol;
}

export interface Route {
  path: string;
  backends: Backend[];
  load_balancing: LoadBalancing;
}

export interface ProxyTlsConfig {
  enabled: boolean;
  acme: boolean;
  domain?: string;
}

export type CertSourceKind = 'exact' | 'wildcard_reuse' | 'pending_issuance' | 'none';

export interface RuleCertStatus {
  source: CertSourceKind;
  covering_domain: string;
  last_updated: string; // RFC3339
}

export interface ProxyRule {
  id: string;
  name: string;
  type: RuleType;
  listen: string;
  domains: string[];
  routes: Route[];
  tls?: ProxyTlsConfig;
  enabled: boolean;
  created_at?: string;
  cert_status?: RuleCertStatus | null;
}

export interface CreateProxyRuleRequest {
  name: string;
  type: RuleType;
  listen: string;
  domains?: string[];
  routes?: Route[];
  tls?: ProxyTlsConfig;
  enabled: boolean;
}

export type UpdateProxyRuleRequest = CreateProxyRuleRequest;

export interface Client {
  name: string;
  hostname: string | null;
  note: string | null;
  online: boolean;
  connected_at: string | null;
  last_seen_at: string;
  first_seen_at: string;
  client_version: string | null;
  referenced_by_rules: number;
}

export interface ServerAuthView {
  client_token: string;
  updated_at: string;
}

// === ACME Certificate Management ===

export type CertificateStatus = 'pending' | 'active' | 'expired' | 'failed';

export interface AcmeStatus {
  enabled: boolean;
  server_url: string;
  cert_dir: string;
  certificate_count: number;
  consumers?: CertConsumers;
}

export interface AcmeCertificate {
  domain: string;
  status: CertificateStatus;
  issued_at?: string;
  expires_at?: string;
  auto_renew: boolean;
  error?: string;
}

export interface AcmeConfig {
  enabled: boolean;
  server_url: string;
  email: string;
  cert_dir: string;
  auto_renew: boolean;
  renewal_check_interval: number;
  renewal_days_before_expiry: number;
  tos_agreed: boolean;
}

export interface UpdateAcmeConfigRequest {
  enabled?: boolean;
  server_url?: string;
  email?: string;
  auto_renew?: boolean;
  renewal_check_interval?: number;
  renewal_days_before_expiry?: number;
  tos_agreed?: boolean;
}

// === DNS Provider Configuration ===

export type DnsProviderType = 'cloudflare' | 'aliyun' | 'tencent' | 'custom';

export interface DnsProviderConfig {
  provider: DnsProviderType;
  api_key: string;
  api_secret?: string;
  zone_id?: string;
}

// === Challenge Status ===

export type ChallengeType = 'http-01' | 'dns-01';

export type ChallengeStatusType = 'pending' | 'verified' | 'failed';

export interface ChallengeStatus {
  domain: string;
  status: ChallengeStatusType;
  type: ChallengeType;
  error?: string;
}

// === Certificate Consumer Binding ===

export interface CertConsumers {
  api_tls: boolean;
  trojan: boolean;
  control_tls: boolean;
  reverse_proxy: boolean;
}

// === Settings ===

export interface GeneralSettings {
  log_level: string;
  api_tls?: boolean;
  api_domain?: string;
  reverse_proxy: ReverseProxySettings;
}

export interface ReverseProxySettings {
  max_connections: number;
  connection_timeout_secs: number;
  buffer_size: number;
}

export interface DnsSettings {
  tunnel_domain: string;
  mesh_domain: string;
}

// === Unified Stats ===

export interface StatsSnapshot {
  entity_type: 'client' | 'proxy' | 'shadowsocks' | 'trojan';
  entity_id: string;
  timestamp: string;
  bytes_in: number;
  bytes_out: number;
  bytes_in_rate: number;
  bytes_out_rate: number;
  rtt_ms: number | null;
  loss_pct: number | null;
  active_conns: number;
}

export interface EntitySummary {
  total_bytes_in: number;
  total_bytes_out: number;
  total_conns: number;
  entity_count: number;
}

export interface StatsSummary {
  clients: EntitySummary;
  proxy: EntitySummary;
  shadowsocks: EntitySummary;
  trojan: EntitySummary;
}

// === LLM Gateway ===

export type ProviderType = 'deepseek' | 'volcengine' | 'kimi' | 'mimo';

export interface LlmGatewayConfig {
  enabled: boolean;
  openai_domain: string | null;
  anthropic_domain: string | null;
  listen: string;
  tls_enabled: boolean;
  tls_acme: boolean;
}

export interface LlmProvider {
  id: string;
  name: string;
  provider_type: ProviderType;
  base_url: string;
  extra_config?: string | null;
  anthropic_base_url?: string | null;
  enabled: boolean;
  created_at: string;
  updated_at: string;
}

export interface CreateProviderRequest {
  name: string;
  provider_type: ProviderType;
  base_url: string;
  api_key: string;
  extra_config?: string | null;
  anthropic_base_url?: string | null;
}

export interface LlmModel {
  id: string;
  provider_id: string;
  model_name: string;
  alias: string;
  tags: string[];
  enabled: boolean;
  extra_config?: string | null;
  created_at: string;
  updated_at: string;
}

export interface CreateModelRequest {
  model_name: string;
  alias?: string;
  tags?: string[];
  extra_config?: string | null;
}

export interface LlmApiKey {
  id: string;
  key_prefix: string;
  name: string;
  enabled: boolean;
  created_at: string;
  last_used_at?: string | null;
  /** 绑定的 RAG 知识库 id（未绑定为 null）。 */
  kb_id?: string | null;
}

export interface CreateApiKeyResponse {
  id: string;
  key: string;
  key_prefix: string;
  name: string;
}

// === LLM Usage Stats ===

export type UsageGroupBy = 'api_key' | 'model' | 'provider';

export interface LlmUsageSummary {
  requests: number;
  success: number;
  prompt_tokens: number;
  cache_hit_tokens: number;
  cache_miss_tokens: number;
  completion_tokens: number;
  total_tokens: number;
}

export interface LlmUsageAggregateRow {
  dimension_id: string | null;
  dimension_name: string;
  requests: number;
  success: number;
  prompt_tokens: number;
  cache_hit_tokens: number;
  cache_miss_tokens: number;
  completion_tokens: number;
  total_tokens: number;
}

export interface LlmUsageLog {
  id: string;
  timestamp: string;
  api_key_id: string | null;
  api_key_name: string;
  provider_id: string | null;
  provider_name: string;
  model_id: string | null;
  model_name: string;
  requested_model: string;
  protocol: string;
  stream: number;
  status_code: number;
  success: number;
  prompt_tokens: number;
  cache_hit_tokens: number;
  cache_miss_tokens: number;
  completion_tokens: number;
  total_tokens: number;
  latency_ms: number;
  error_type: string | null;
  /** 本请求从哪个模型故障转移而来（未转移为 null）。 */
  failover_from?: string | null;
}

export interface LlmUsageLogsResponse {
  logs: LlmUsageLog[];
  total: number;
}

// === LLM 模型组（多模型故障转移） ===

export interface LlmModelGroup {
  id: string;
  name: string;
  enabled: boolean;
  member_count: number;
  created_at: string;
  updated_at: string;
}

export interface BreakerSnapshot {
  state: string; // "closed" | "open" | "halfopenprobe"
  consecutive_failures: number;
  cooldown_remaining_secs: number;
}

export interface LlmGroupMember {
  model_id: string;
  priority: number;
  model_name: string;
  alias: string;
  provider_id: string;
  provider_name: string;
  model_enabled: boolean;
  breaker: BreakerSnapshot;
}

export interface LlmModelGroupDetail {
  id: string;
  name: string;
  enabled: boolean;
  created_at: string;
  updated_at: string;
  members: LlmGroupMember[];
}

// === LLM RAG Knowledge Base ===

export type KbDocStatus = 'pending' | 'processing' | 'ready' | 'failed';

export interface LlmKnowledgeBase {
  id: string;
  name: string;
  description: string;
  emb_base_url: string;
  emb_api_key: string; // 恒为空字符串（后端不回显）
  emb_model: string;
  emb_dimension: number;
  top_k: number;
  chunk_size: number;
  chunk_overlap: number;
  score_threshold: number;
  enabled: boolean;
  doc_count?: number;
  chunk_count?: number;
  created_at: string;
  updated_at: string;
}

export interface LlmKbDocument {
  id: string;
  kb_id: string;
  filename: string;
  file_type: string;
  content_hash: string;
  status: KbDocStatus;
  chunk_count: number;
  error?: string | null;
  created_at: string;
  updated_at: string;
}

export interface KbQueryHit {
  heading_path: string;
  content: string;
  score: number;
}

export interface KbQueryResult {
  chunks: KbQueryHit[];
}

export interface KbEvent {
  doc_id: string;
  kb_id: string;
  status: KbDocStatus;
  chunk_count: number;
  error?: string | null;
}

export interface CreateLlmKbRequest {
  name: string;
  description: string;
  emb_base_url: string;
  emb_api_key: string;
  emb_model: string;
  emb_dimension: number;
  top_k?: number;
  chunk_size?: number;
  chunk_overlap?: number;
  score_threshold?: number;
  enabled?: boolean;
}

export interface UpdateLlmKbRequest {
  name: string;
  description: string;
  top_k?: number;
  chunk_size?: number;
  chunk_overlap?: number;
  score_threshold?: number;
}

export interface TestEmbeddingResult {
  dimension: number;
  latency_ms: number;
}

// === Agent Workbench ===

export interface AgentWorkspace {
  id: string;
  name: string;
  client_id: string;
  runtime_type: 'host' | 'docker';
  root_path: string;
  docker_image?: string;
  docker_container_id?: string;
  approval_mode: 'safe' | 'auto_write' | 'full_auto';
  system_prompt: string | null;
  /** ACP 远程 agent 引擎：空串（缺省）= 内置 runner；非空 = gemini/claude-code/opencode */
  agent_type?: '' | 'gemini' | 'claude-code' | 'opencode';
  /** ACP agent 可执行文件路径；缺省依赖 PATH 查找 */
  agent_path?: string;
  /** workspace 默认 LLM 模型 id（llm_models.id） */
  llm_model_id?: string;
  created_at: string;
  updated_at: string;
}

export interface AgentSession {
  id: string;
  workspace_id: string;
  title?: string;
  status: 'active' | 'archived';
  model?: string;
  created_at: string;
  updated_at: string;
}

export interface AgentMessage {
  id: string;
  session_id: string;
  role: string;
  content: string;
  tool_calls?: string | null;
  tool_call_id?: string | null;
  name?: string | null;
  kind: string;
  created_at: string;
}

export interface SessionConfigSelectOption {
  value: string;
  name: string;
  description?: string;
}

export interface SessionConfigOption {
  id: string;
  name: string;
  description?: string;
  category?: 'mode' | 'model' | 'model_config' | 'thought_level' | string;
  type: 'select' | 'boolean';
  /** select 时为当前 value-id；boolean 时归一化为 "true"/"false"，真值见 currentBool */
  currentValue?: string | boolean;
  /** select 的可选项（ungrouped 平铺；grouped 形态拍平后填入） */
  options?: SessionConfigSelectOption[];
  currentBool?: boolean;
}

export type AgentWsEvent =
  | { type: 'assistant_chunk'; content?: string; final?: boolean; thought?: boolean }
  | {
      type: 'tool_call';
      id?: string;
      name?: string;
      args?: string;
      status?: 'pending' | 'in_progress' | 'running' | 'completed' | 'failed';
      tool_kind?: ToolKind;
      diffs?: ToolDiff[];
      locations?: ToolLocation[];
    }
  | {
      type: 'tool_result';
      id?: string;
      name?: string;
      result?: string;
      // claude-code-acp 的 ToolCall 首帧 rawInput 常是 {}，真正的参数经
      // ToolCallUpdate.rawInput 到达后由本帧携带——前端需合并进卡片 args
      args?: string;
      status?: 'pending' | 'in_progress' | 'running' | 'completed' | 'failed';
      tool_kind?: ToolKind;
      diffs?: ToolDiff[];
      locations?: ToolLocation[];
    }
  | { type: 'plan'; entries?: PlanEntryItem[] }
  | { type: 'usage'; used?: number; size?: number }
  | { type: 'status'; message?: string }
  | { type: 'done' }
  | { type: 'stopped' }
  | { type: 'session_title'; title?: string; session_id?: string }
  | { type: 'approval_request'; request_id: string; tool: string; summary: string; args_preview: string }
  | { type: 'error'; message?: string }
  // 上游流传输失败重试信号：前端应丢弃已缓冲的半截增量，等重试后的完整文本从新气泡开始
  | { type: 'stream_reset' }
  | { type: 'session_state'; options?: SessionConfigOption[] }
  | { type: 'config_option_update'; options?: SessionConfigOption[] }
  | { type: 'current_mode_update'; mode_id?: string };

export interface FsEntry {
  name: string;
  is_dir: boolean;
}

export interface FsFileContent {
  content: string;
  truncated: boolean;
}

export interface GitStatusResult {
  status: string;
  stderr: string;
}

export interface GitDiffResult {
  diff: string;
}


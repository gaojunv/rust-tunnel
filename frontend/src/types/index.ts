// Connection quality data
export interface ConnectionQuality {
  last_rtt_ms: number;
  avg_rtt_ms: number;
  min_rtt_ms: number;
  max_rtt_ms: number;
  loss_rate: number;
  consecutive_losses: number;
  bytes_in_per_sec: number;
  bytes_out_per_sec: number;
  quality_score: number;
  last_update: string;
  is_warning: boolean;
  is_critical: boolean;
}

// Historical quality sample
export interface QualitySample {
  timestamp: string;
  avg_rtt_ms: number;
  loss_rate: number;
  bytes_in_per_sec: number;
  bytes_out_per_sec: number;
  quality_score: number;
}

// Client with quality data
export interface ClientWithQuality {
  port: number;
  hostname?: string;
  quality: ConnectionQuality;
}

// Port quality response with history
export interface PortQualityResponse {
  current: ConnectionQuality;
  history: QualitySample[];
}

// Quality warning
export interface QualityWarning {
  port: number;
  hostname?: string;
  quality: ConnectionQuality;
  warning_type: string;
}

export interface TrafficBucket {
  timestamp: string;
  bytes_in: number;
  bytes_out: number;
}

export interface PortTraffic {
  port: number;
  total_bytes_in: number;
  total_bytes_out: number;
  buckets: TrafficBucket[];
}

export interface ServerMetrics {
  client_count: number;
  active_connection_count: number;
  total_bytes_in: number;
  total_bytes_out: number;
}

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

// Shadowsocks statistics
export interface ShadowsocksStats {
  enabled: boolean;
  port?: number;
  total_bytes_in: number;
  total_bytes_out: number;
  active_connections: number;
}

// Shadowsocks quality data with history
export interface ShadowsocksQuality {
  port: number;
  quality: ConnectionQuality;
  history: QualitySample[];
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

// Trojan statistics
export interface TrojanStats {
  enabled: boolean;
  port?: number;
  total_bytes_in: number;
  total_bytes_out: number;
  active_connections: number;
}

// Trojan quality data with history
export interface TrojanQuality {
  port: number;
  quality: ConnectionQuality;
  history: QualitySample[];
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

export interface ProxyStats {
  total_rules: number;
  active_rules: number;
  total_connections: number;
  bytes_in: number;
  bytes_out: number;
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

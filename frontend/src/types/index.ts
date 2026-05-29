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

export interface ClientResponse {
  port: number;
  hostname?: string;
  connection_count: number;
  quality?: ConnectionQuality;
}

export interface ClientGroup {
  hostname: string;
  clients: ClientResponse[];
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

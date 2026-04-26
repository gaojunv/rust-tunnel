export interface ClientResponse {
  port: number;
  connection_count: number;
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

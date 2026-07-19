import axios from 'axios';
import type {
  ClientResponse,
  PortTraffic,
  ServerMetrics,
  LoginRequest,
  LoginResponse,
  QualitySample,
  ClientWithQuality,
  PortQualityResponse,
  QualityWarning,
  ShadowsocksConfig,
  ShadowsocksStats,
  ShadowsocksQuality,
  TrojanConfig,
  TrojanStats,
  TrojanQuality,
  LogEntry,
  MeshNetworkResponse,
  MeshServiceResponse,
  DnsRecordResponse,
  AddDnsRecordRequest,
  ProxyRule,
  ProxyStats,
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
} from '../types';

const API_BASE = '/api';

// Create axios instance
const api = axios.create({
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

// Clients API
export const getClients = async (): Promise<ClientResponse[]> => {
  const response = await api.get<ClientResponse[]>('/clients');
  return response.data;
};

export const disconnectClient = async (port: number): Promise<void> => {
  await api.delete(`/clients/${port}`);
};

// Traffic API
export const getTraffic = async (): Promise<PortTraffic[]> => {
  const response = await api.get<PortTraffic[]>('/traffic');
  return response.data;
};

export const getPortTraffic = async (port: number): Promise<PortTraffic> => {
  const response = await api.get<PortTraffic>(`/traffic/${port}`);
  return response.data;
};

// Metrics API
export const getMetrics = async (): Promise<ServerMetrics> => {
  const response = await api.get<ServerMetrics>('/metrics');
  return response.data;
};

// Health check
export const checkHealth = async (): Promise<{ status: string }> => {
  const response = await api.get('/health');
  return response.data;
};

// Quality API
export const getAllQuality = async (): Promise<ClientWithQuality[]> => {
  const response = await api.get<ClientWithQuality[]>('/quality/all');
  return response.data;
};

export const getPortQuality = async (port: number): Promise<PortQualityResponse> => {
  const response = await api.get<PortQualityResponse>(`/quality/${port}`);
  return response.data;
};

export const getQualityHistory = async (port: number): Promise<QualitySample[]> => {
  const response = await api.get<QualitySample[]>(`/quality/${port}/history`);
  return response.data;
};

export const getQualityWarnings = async (): Promise<QualityWarning[]> => {
  const response = await api.get<QualityWarning[]>('/quality/warnings');
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

export const getShadowsocksStats = async (): Promise<ShadowsocksStats> => {
  const response = await api.get<ShadowsocksStats>('/shadowsocks/stats');
  return response.data;
};

export const getShadowsocksQuality = async (): Promise<ShadowsocksQuality[]> => {
  const response = await api.get<ShadowsocksQuality[]>('/shadowsocks/quality');
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

export const getTrojanStats = async (): Promise<TrojanStats> => {
  const response = await api.get<TrojanStats>('/trojan/stats');
  return response.data;
};

export const getTrojanQuality = async (): Promise<TrojanQuality[]> => {
  const response = await api.get<TrojanQuality[]>('/trojan/quality');
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

export const getProxyStats = async (): Promise<ProxyStats> => {
  const response = await api.get<ProxyStats>('/proxy/stats');
  return response.data;
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

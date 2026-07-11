import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import {
  checkHealth,
  getClients,
  getMetrics,
  getTraffic,
  getAllQuality,
  getPortQuality,
  getShadowsocksConfig,
  updateShadowsocksConfig,
  getShadowsocksStats,
  getShadowsocksQuality,
  getTrojanConfig,
  updateTrojanConfig,
  getTrojanStats,
  getTrojanQuality,
  getLogs,
  login,
  logout,
  getPortTraffic,
  getQualityHistory,
  getQualityWarnings,
  getMeshes,
  getMeshServices,
  getDnsRecords,
  addDnsRecord,
  deleteDnsRecord,
  getLogsLevel,
  setLogsLevel,
  disconnectClient,
} from './client';
import type {
  LoginRequest,
  ShadowsocksConfig,
  TrojanConfig,
} from '../types';

// Health check
export function useHealth() {
  return useQuery({
    queryKey: ['health'],
    queryFn: checkHealth,
  });
}

// Clients
export function useClients() {
  return useQuery({
    queryKey: ['clients'],
    queryFn: getClients,
    refetchInterval: 5000,
  });
}

// Metrics
export function useMetrics() {
  return useQuery({
    queryKey: ['metrics'],
    queryFn: getMetrics,
    refetchInterval: 5000,
  });
}

// Traffic
export function useTraffic() {
  return useQuery({
    queryKey: ['traffic'],
    queryFn: getTraffic,
    refetchInterval: 5000,
  });
}

export function usePortTraffic(port?: number) {
  return useQuery({
    queryKey: ['portTraffic', port],
    queryFn: () => getPortTraffic(port!),
    enabled: port !== undefined,
    refetchInterval: 5000,
  });
}

// Quality
export function useAllQuality() {
  return useQuery({
    queryKey: ['allQuality'],
    queryFn: getAllQuality,
    refetchInterval: 5000,
  });
}

export function usePortQuality(port?: number) {
  return useQuery({
    queryKey: ['portQuality', port],
    queryFn: () => getPortQuality(port!),
    enabled: port !== undefined,
    refetchInterval: 5000,
  });
}

export function useQualityHistory(port?: number) {
  return useQuery({
    queryKey: ['qualityHistory', port],
    queryFn: () => getQualityHistory(port!),
    enabled: port !== undefined,
  });
}

export function useQualityWarnings() {
  return useQuery({
    queryKey: ['qualityWarnings'],
    queryFn: getQualityWarnings,
    refetchInterval: 5000,
  });
}

// Shadowsocks
export function useShadowsocksConfig() {
  return useQuery({
    queryKey: ['shadowsocks-config'],
    queryFn: getShadowsocksConfig,
    refetchInterval: 5000,
  });
}

export function useShadowsocksStats() {
  return useQuery({
    queryKey: ['shadowsocks-stats'],
    queryFn: getShadowsocksStats,
    refetchInterval: 5000,
  });
}

export function useShadowsocksQuality() {
  return useQuery({
    queryKey: ['shadowsocks-quality'],
    queryFn: getShadowsocksQuality,
    refetchInterval: 5000,
  });
}

export function useUpdateShadowsocksConfig() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (config: Partial<ShadowsocksConfig>) => updateShadowsocksConfig(config),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['shadowsocks-config'] });
    },
  });
}

// Trojan
export function useTrojanConfig() {
  return useQuery({
    queryKey: ['trojan-config'],
    queryFn: getTrojanConfig,
    refetchInterval: 5000,
  });
}

export function useTrojanStats() {
  return useQuery({
    queryKey: ['trojan-stats'],
    queryFn: getTrojanStats,
    refetchInterval: 5000,
  });
}

export function useTrojanQuality() {
  return useQuery({
    queryKey: ['trojan-quality'],
    queryFn: getTrojanQuality,
    refetchInterval: 5000,
  });
}

export function useUpdateTrojanConfig() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (config: Partial<TrojanConfig>) => updateTrojanConfig(config),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['trojan-config'] });
    },
  });
}

// Logs
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

export function useLogsLevel() {
  return useQuery({
    queryKey: ['logsLevel'],
    queryFn: getLogsLevel,
  });
}

export function useSetLogsLevel() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (level: string) => setLogsLevel(level),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['logsLevel'] });
    },
  });
}

// Mesh
export function useMeshes() {
  return useQuery({
    queryKey: ['meshes'],
    queryFn: getMeshes,
    refetchInterval: 10000,
  });
}

export function useMeshServices(id?: string) {
  return useQuery({
    queryKey: ['mesh-services', id],
    queryFn: () => getMeshServices(id!),
    enabled: !!id,
  });
}

// DNS
export function useDnsRecords() {
  return useQuery({
    queryKey: ['dns-records'],
    queryFn: getDnsRecords,
    refetchInterval: 15000,
  });
}

export function useAddDnsRecord() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (data: { name: string; record_type: string; value: string; port?: number }) =>
      addDnsRecord(data),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['dns-records'] });
    },
  });
}

export function useDeleteDnsRecord() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => deleteDnsRecord(name),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['dns-records'] });
    },
  });
}

// Disconnect client
export function useDisconnectClient() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (port: number) => disconnectClient(port),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['clients'] });
      queryClient.invalidateQueries({ queryKey: ['traffic'] });
      queryClient.invalidateQueries({ queryKey: ['metrics'] });
    },
  });
}

// Login
export function useLogin() {
  return useMutation({
    mutationFn: (data: LoginRequest) => login(data),
  });
}

export function useLogout() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () => logout(),
    onSuccess: () => {
      queryClient.clear();
      localStorage.removeItem('auth_token');
    },
  });
}

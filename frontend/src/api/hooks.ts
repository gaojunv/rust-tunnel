import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import {
  login,
  getClients,
  getMetrics,
  getPortQuality,
  getPortTraffic,
  getAllQuality,
  getShadowsocksConfig,
  updateShadowsocksConfig,
  getShadowsocksStats,
  getShadowsocksQuality,
} from './client';
import type { LoginRequest } from '../types';

export function useClients() {
  return useQuery({
    queryKey: ['clients'],
    queryFn: () => getClients(),
    refetchInterval: 5000,
  });
}

export function useMetrics() {
  return useQuery({
    queryKey: ['metrics'],
    queryFn: () => getMetrics(),
    refetchInterval: 5000,
  });
}

export function useQuality(port: number) {
  return useQuery({
    queryKey: ['quality', port],
    queryFn: () => getPortQuality(port),
    enabled: port > 0,
    refetchInterval: 10000,
  });
}

export function useTraffic(port: number, _hours = 24) {
  return useQuery({
    queryKey: ['traffic', port, _hours],
    queryFn: () => getPortTraffic(port),
    enabled: port > 0,
  });
}

export function useQualitySummary() {
  return useQuery({
    queryKey: ['quality-summary'],
    queryFn: async () => {
      const clients = await getAllQuality();
      const totalConnections = clients.length;
      const warningCount = clients.filter(
        (c) => c.quality.is_warning || c.quality.is_critical
      ).length;
      const averageScore =
        totalConnections > 0
          ? clients.reduce((sum, c) => sum + c.quality.quality_score, 0) / totalConnections
          : 0;

      const mappedClients = clients.map((c) => ({
        port: c.port,
        hostname: c.hostname,
        score: c.quality.quality_score,
        rtt: c.quality.avg_rtt_ms,
        loss: c.quality.loss_rate * 100,
        is_warning: c.quality.is_warning,
        is_critical: c.quality.is_critical,
      }));

      const worst = [...mappedClients]
        .sort((a, b) => a.score - b.score)
        .slice(0, 10);

      return {
        total_connections: totalConnections,
        warning_count: warningCount,
        average_score: averageScore,
        clients: mappedClients,
        worst,
      };
    },
    refetchInterval: 10000,
  });
}

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
      queryClient.invalidateQueries({ queryKey: ['shadowsocks-stats'] });
    },
  });
}

export function useShadowsocksStats() {
  return useQuery({
    queryKey: ['shadowsocks-stats'],
    queryFn: () => getShadowsocksStats(),
    refetchInterval: 5000,
  });
}

export function useShadowsocksQuality() {
  return useQuery({
    queryKey: ['shadowsocks-quality'],
    queryFn: () => getShadowsocksQuality(),
    refetchInterval: 10000,
  });
}

export function useLogin() {
  return useMutation({
    mutationFn: (password: string) => login({ password } as LoginRequest),
  });
}

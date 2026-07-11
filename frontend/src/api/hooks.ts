import { useQuery, useMutation } from '@tanstack/react-query';
import { login, getClients, getMetrics, getPortQuality, getPortTraffic } from './client';
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

export function useLogin() {
  return useMutation({
    mutationFn: (password: string) => login({ password } as LoginRequest),
  });
}

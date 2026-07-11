import { useQuery, useMutation } from '@tanstack/react-query';
import { login, getClients, getMetrics } from './client';
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

export function useLogin() {
  return useMutation({
    mutationFn: (password: string) => login({ password } as LoginRequest),
  });
}

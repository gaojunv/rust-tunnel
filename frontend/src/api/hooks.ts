import { useMutation } from '@tanstack/react-query';
import { login } from './client';
import type { LoginRequest } from '../types';

export function useLogin() {
  return useMutation({
    mutationFn: (password: string) => login({ password } as LoginRequest),
  });
}

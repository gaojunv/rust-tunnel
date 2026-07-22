// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';
import { renderHook } from '@testing-library/react';
import { useEntityLabel } from './useEntityLabel';

// Mock useProxyRules —— Task 1 中让它返回空数组
vi.mock('@/api/hooks', () => ({
  useProxyRules: () => ({ data: [] as { id: string; name: string }[] }),
}));

describe('useEntityLabel', () => {
  it('returns client_name as-is for client entity', () => {
    const { result } = renderHook(() => useEntityLabel());
    expect(result.current('client', 'home-nas')).toBe('home-nas');
  });

  it('formats shadowsocks entity id with port', () => {
    const { result } = renderHook(() => useEntityLabel());
    expect(result.current('shadowsocks', 'shadowsocks:8388')).toBe('Shadowsocks (port 8388)');
  });

  it('formats trojan entity id with port', () => {
    const { result } = renderHook(() => useEntityLabel());
    expect(result.current('trojan', 'trojan:8443')).toBe('Trojan (port 8443)');
  });

  it('falls back to raw id when shadowsocks id has no valid port', () => {
    const { result } = renderHook(() => useEntityLabel());
    expect(result.current('shadowsocks', 'shadowsocks:')).toBe('shadowsocks:');
    expect(result.current('shadowsocks', 'shadowsocks:not-a-port')).toBe('shadowsocks:not-a-port');
  });

  it('falls back to raw id when trojan id has no valid port', () => {
    const { result } = renderHook(() => useEntityLabel());
    expect(result.current('trojan', 'trojan:xyz')).toBe('trojan:xyz');
  });
});
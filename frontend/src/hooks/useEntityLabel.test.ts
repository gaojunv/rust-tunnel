// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { renderHook } from '@testing-library/react';
import { useEntityLabel } from './useEntityLabel';

const mockRules = vi.hoisted(() => ({
  current: [] as { id: string; name: string }[],
}));

vi.mock('@/api/hooks', () => ({
  useProxyRules: () => ({ data: mockRules.current }),
}));

beforeEach(() => {
  mockRules.current = [];
});

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

  it('maps proxy id to rule.name when rule is found', () => {
    mockRules.current = [
      { id: 'r-a3f2c1', name: 'My NAS Reverse' },
      { id: 'r-b7e9d0', name: 'Other Rule' },
    ];
    const { result } = renderHook(() => useEntityLabel());
    expect(result.current('proxy', 'r-a3f2c1')).toBe('My NAS Reverse');
    expect(result.current('proxy', 'r-b7e9d0')).toBe('Other Rule');
  });

  it('falls back to truncated id when proxy rule not found', () => {
    mockRules.current = [{ id: 'r-a3f2c1', name: 'NAS' }];
    const { result } = renderHook(() => useEntityLabel());
    // 未命中：显示 id 前 8 字符 + '…'（如果超过 8 字符），否则原样
    expect(result.current('proxy', 'unknown-id-12345')).toBe('unknown-…');
    expect(result.current('proxy', 'short')).toBe('short');
  });

  it('disambiguates duplicate proxy names with id suffix', () => {
    mockRules.current = [
      { id: 'a1b2c3d4', name: 'NAS' },
      { id: 'e5f6g7h8', name: 'NAS' },
      { id: 'i9j0k1l2', name: 'Unique' },
    ];
    const { result } = renderHook(() => useEntityLabel());
    // 第一条保持原 name；第二条起追加 (id 前 6 字符)
    expect(result.current('proxy', 'a1b2c3d4')).toBe('NAS');
    expect(result.current('proxy', 'e5f6g7h8')).toBe('NAS (e5f6g7)');
    expect(result.current('proxy', 'i9j0k1l2')).toBe('Unique');
  });
});
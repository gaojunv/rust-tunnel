import { useCallback } from 'react';
import { useProxyRules } from '@/api/hooks';
import type { StatsSnapshot } from '@/types';

export type EntityType = StatsSnapshot['entity_type'];

/**
 * 返回 (entity_type, entity_id) → 人类可读 label 的映射函数。
 *
 * 映射规则：
 *  - client:      原样返回 entity_id（即 client_name）
 *  - proxy:       从 useProxyRules() 查 rule.name；重名时第二条起追加 (id 前 6 字符)；
 *                 未命中则显示截断的 id
 *  - shadowsocks: "shadowsocks:<port>" → "Shadowsocks (port <port>)"，无法解析端口则原样
 *  - trojan:      "trojan:<port>" → "Trojan (port <port>)"，无法解析端口则原样
 */
export function useEntityLabel(): (type: EntityType, id: string) => string {
  // Task 1 暂不使用 rules，Task 2 补上
  useProxyRules();

  return useCallback((type, id) => {
    switch (type) {
      case 'client':
        return id;
      case 'shadowsocks':
        return formatPortLabel(id, 'shadowsocks:', 'Shadowsocks');
      case 'trojan':
        return formatPortLabel(id, 'trojan:', 'Trojan');
      case 'proxy':
        // Task 2 中实现
        return id;
      default:
        return id;
    }
  }, []);
}

function formatPortLabel(id: string, prefix: string, kindLabel: string): string {
  if (!id.startsWith(prefix)) return id;
  const portStr = id.slice(prefix.length);
  const port = Number.parseInt(portStr, 10);
  if (!Number.isFinite(port) || String(port) !== portStr || port <= 0) {
    return id;
  }
  return `${kindLabel} (port ${port})`;
}
import { useCallback, useMemo } from 'react';
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
  const { data: rules } = useProxyRules();

  // 预计算：proxy id → 最终 label（含重名去重）
  const proxyLabels = useMemo(() => {
    const map = new Map<string, string>();
    if (!rules) return map;

    // 按 name 分组
    const byName = new Map<string, string[]>();
    for (const r of rules) {
      const ids = byName.get(r.name) ?? [];
      ids.push(r.id);
      byName.set(r.name, ids);
    }

    // 生成最终 label
    for (const r of rules) {
      const sameName = byName.get(r.name) ?? [];
      if (sameName.length <= 1) {
        map.set(r.id, r.name);
      } else {
        const idx = sameName.indexOf(r.id);
        if (idx === 0) {
          map.set(r.id, r.name);
        } else {
          map.set(r.id, `${r.name} (${r.id.slice(0, 6)})`);
        }
      }
    }
    return map;
  }, [rules]);

  return useCallback(
    (type, id) => {
      switch (type) {
        case 'client':
          return id;
        case 'shadowsocks':
          return formatPortLabel(id, 'shadowsocks:', 'Shadowsocks');
        case 'trojan':
          return formatPortLabel(id, 'trojan:', 'Trojan');
        case 'proxy': {
          const mapped = proxyLabels.get(id);
          if (mapped) return mapped;
          // 未命中：id 超过 8 字符则截断加省略号
          return id.length > 8 ? `${id.slice(0, 8)}…` : id;
        }
        default:
          return id;
      }
    },
    [proxyLabels],
  );
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
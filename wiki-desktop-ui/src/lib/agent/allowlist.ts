/**
 * 会话级审批 allowlist —— 纯函数，便于单测
 * key 设计：method + 首词/路径模式
 * 命中时前端自动 `agentRespond` 批准，不再弹窗
 */

export type Allowlist = Set<string>;

/** 创建空 allowlist */
export function createAllowlist(): Allowlist {
  return new Set<string>();
}

/** 提取命令首词（取第一段空白分隔 token，去掉前导路径） */
export function firstTokenOfCommand(command: string | null | undefined): string {
  if (!command) return "*";
  const trimmed = command.trim();
  if (!trimmed) return "*";
  // 取首段 token，以空白分隔（简化，不处理引号）
  const first = trimmed.split(/\s+/)[0] ?? "*";
  // 去掉路径前缀，取 basename
  const base = first.split("/").pop() ?? first;
  return base || "*";
}

/**
 * 按 method+params 生成 allowlist key
 * - commandExecution: method + 首词
 * - fileChange: method + grantRoot 或 "*"
 * - permissions: method + "*"
 * - legacy execCommandApproval: method + 首词
 * - legacy applyPatchApproval: method + "*"
 */
export function buildAllowlistKey(method: string, params: unknown): string {
  const p = (params ?? {}) as Record<string, unknown>;
  switch (method) {
    case "item/commandExecution/requestApproval": {
      const cmd = typeof p["command"] === "string" ? (p["command"] as string) : null;
      return `${method}:${firstTokenOfCommand(cmd)}`;
    }
    case "item/fileChange/requestApproval": {
      const grantRoot = typeof p["grantRoot"] === "string" ? (p["grantRoot"] as string) : null;
      return `${method}:${grantRoot ?? "*"}`;
    }
    case "item/permissions/requestApproval": {
      return `${method}:*`;
    }
    case "execCommandApproval": {
      const cmdArr = p["command"] as unknown;
      let first = "*";
      if (Array.isArray(cmdArr) && cmdArr.length > 0 && typeof cmdArr[0] === "string") {
        first = firstTokenOfCommand(cmdArr[0] as string);
      }
      return `${method}:${first}`;
    }
    case "applyPatchApproval": {
      // fileChanges 为 map，取首个路径的目录作 key 更细，但简化用 "*"
      return `${method}:*`;
    }
    default:
      return `${method}:*`;
  }
}

export function allowlistHas(list: Allowlist, method: string, params: unknown): boolean {
  const key = buildAllowlistKey(method, params);
  return list.has(key);
}

export function allowlistAdd(list: Allowlist, method: string, params: unknown): void {
  const key = buildAllowlistKey(method, params);
  list.add(key);
}

export function allowlistRemove(list: Allowlist, key: string): void {
  list.delete(key);
}

export function allowlistClear(list: Allowlist): void {
  list.clear();
}

export function allowlistKeys(list: Allowlist): string[] {
  return Array.from(list);
}

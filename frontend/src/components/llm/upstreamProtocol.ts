export type UpstreamProtocol = 'chat_completions' | 'responses';

/** 从 extra_config JSON 读 upstream_protocol；缺省/非法返回 'chat_completions'。 */
export function parseUpstreamProtocol(extraConfig?: string | null): UpstreamProtocol {
  if (!extraConfig) return 'chat_completions';
  try {
    const v = (JSON.parse(extraConfig) as { upstream_protocol?: unknown }).upstream_protocol;
    if (v === 'responses') return 'responses';
    return 'chat_completions';
  } catch {
    return 'chat_completions';
  }
}

/**
 * 把协议选项合并回 extra_config JSON；
 * 'chat_completions' 删键（默认保持配置干净），保留其他键。
 */
export function mergeUpstreamProtocol(
  extraConfig: string | null | undefined,
  proto: UpstreamProtocol,
): string | null {
  let obj: Record<string, unknown> = {};
  if (extraConfig) {
    try {
      obj = JSON.parse(extraConfig) as Record<string, unknown>;
    } catch {
      obj = {};
    }
  }
  if (proto === 'responses') {
    obj.upstream_protocol = 'responses';
  } else {
    delete obj.upstream_protocol;
  }
  return Object.keys(obj).length ? JSON.stringify(obj) : null;
}

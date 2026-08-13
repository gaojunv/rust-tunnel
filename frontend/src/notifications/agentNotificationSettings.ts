import type { AgentNotification } from '../types';

const NOTIFICATIONS_ENABLED_KEY = 'agent.notificationsEnabled';

/** 本模块只用到 Storage 的 getItem/setItem（测试传最小替身，无需完整 Storage）。 */
type StorageLike = Pick<Storage, 'getItem' | 'setItem'>;

function safeStorage(): StorageLike | undefined {
  try {
    return typeof window !== 'undefined' ? window.localStorage : undefined;
  } catch {
    return undefined;
  }
}

/** 通知开关（默认开启）。localStorage 缺失/损坏时回退默认。 */
export function getNotificationsEnabled(
  storage: StorageLike | undefined = safeStorage(),
): boolean {
  if (!storage) return true;
  try {
    const raw = storage.getItem(NOTIFICATIONS_ENABLED_KEY);
    if (raw === null) return true;
    if (raw === '1' || raw === 'true') return true;
    if (raw === '0' || raw === 'false') return false;
    return true; // 无法识别的值视为损坏，回退默认
  } catch {
    return true;
  }
}

export function setNotificationsEnabled(
  enabled: boolean,
  storage: StorageLike | undefined = safeStorage(),
): void {
  if (!storage) return;
  try {
    storage.setItem(NOTIFICATIONS_ENABLED_KEY, enabled ? '1' : '0');
  } catch {
    /* 存储不可用时静默失败 */
  }
}

/**
 * 判定一条工作台通知是否需要提醒。
 *
 * 规则：开关关闭 → 不提醒；用户正盯着该会话（标签页可见且是该会话）→ 不打扰
 * （任务就在眼前，无需再闪标题/弹系统通知）；其余场景——标签页在后台、或会话
 * 不是当前正在查看的——都提醒（工作区全局，含未查看会话）。
 */
export function shouldNotify(
  ev: AgentNotification,
  opts: { enabled: boolean; activeSessionId: string | null; tabVisible: boolean },
): boolean {
  if (!opts.enabled) return false;
  if (opts.tabVisible && ev.session_id === opts.activeSessionId) return false;
  return true;
}

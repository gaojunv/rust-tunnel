import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from 'react';
import { useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { agentNotificationsWsUrl } from '../api/client';
import type { AgentNotification } from '../types';
import {
  getNotificationsEnabled,
  setNotificationsEnabled,
  shouldNotify,
} from './agentNotificationSettings';

type Permission = 'default' | 'granted' | 'denied';

export interface AgentNotificationsContextValue {
  /** 通知开关（默认开启，localStorage 持久化）。 */
  enabled: boolean;
  /** 浏览器通知授权状态（未授权时仅标签页标题闪烁）。 */
  permission: Permission;
  /** 切换开关；开启时触发 `Notification.requestPermission()`。 */
  setEnabled: (v: boolean) => void;
  /** 上报当前正在查看的会话 id（无会话/离开 Agent 页传 null）。 */
  setActiveSessionId: (id: string | null) => void;
}

const AgentNotificationsContext = createContext<AgentNotificationsContextValue | undefined>(
  undefined,
);

/** 安全读取 Notification API：jsdom/SSR 无该全局时返回 undefined。 */
function getNotificationApi(): (typeof Notification) | undefined {
  return typeof Notification !== 'undefined' ? Notification : undefined;
}

const NOTIFICATION_TAG_PREFIX = 'agent-notif-';

/** 标题闪烁交替周期（ms）。 */
const FLASH_INTERVAL_MS = 1000;
/** 通知 WS 断线重连退避上限（与 ChatStream 一致）。 */
const MAX_RECONNECT_MS = 15000;

export function AgentNotificationsProvider({ children }: { children: ReactNode }) {
  const { t } = useTranslation();
  // t 的身份随语言切换变化：WS effect 依赖稳定回调，语言切换不拆断连接
  // （与 ChatStream 的 tRef 同模式）。
  const tRef = useRef(t);
  useEffect(() => {
    tRef.current = t;
  }, [t]);
  const navigate = useNavigate();

  const [enabled, setEnabledState] = useState<boolean>(() => getNotificationsEnabled());
  const enabledRef = useRef(enabled);
  useEffect(() => {
    enabledRef.current = enabled;
  }, [enabled]);

  const [permission, setPermission] = useState<Permission>(() => {
    const N = getNotificationApi();
    return N ? (N.permission as Permission) : 'default';
  });
  const permissionRef = useRef(permission);
  useEffect(() => {
    permissionRef.current = permission;
  }, [permission]);

  // 当前正在查看的会话（AgentPage 上报；无会话/离开时为 null）。
  const activeSessionIdRef = useRef<string | null>(null);

  // ── 标题闪烁 ────────────────────────────────────────────────
  const originalTitleRef = useRef<string | null>(null);
  const flashTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const stopFlashing = useCallback(() => {
    if (flashTimerRef.current) {
      clearInterval(flashTimerRef.current);
      flashTimerRef.current = null;
    }
    if (originalTitleRef.current != null && typeof document !== 'undefined') {
      document.title = originalTitleRef.current;
    }
    originalTitleRef.current = null;
  }, []);

  /** 标题闪烁：置通知文案，并每秒在「文案 ⇄ 原始标题」间交替；用户回前台即停。 */
  const flashTitle = useCallback(
    (text: string) => {
      if (typeof document === 'undefined') return;
      const original = document.title;
      if (originalTitleRef.current === null) originalTitleRef.current = original;
      document.title = text;
      if (flashTimerRef.current) return; // 已闪烁中：仅更新文案，不重置定时器
      flashTimerRef.current = setInterval(() => {
        const base = originalTitleRef.current ?? original;
        document.title = document.title === text ? base : text;
      }, FLASH_INTERVAL_MS);
    },
    [],
  );

  // 用户回到前台/聚焦窗口：停止闪烁并还原标题。
  useEffect(() => {
    const onVisibility = () => {
      if (document.visibilityState === 'visible') stopFlashing();
    };
    const onFocus = () => stopFlashing();
    document.addEventListener('visibilitychange', onVisibility);
    window.addEventListener('focus', onFocus);
    return () => {
      document.removeEventListener('visibilitychange', onVisibility);
      window.removeEventListener('focus', onFocus);
      stopFlashing();
    };
  }, [stopFlashing]);

  // ── 开关与权限 ─────────────────────────────────────────────
  const setEnabled = useCallback((v: boolean) => {
    setEnabledState(v);
    setNotificationsEnabled(v);
    if (v) {
      const N = getNotificationApi();
      if (N) {
        // 用户手势内请求权限（浏览器要求）；未授权则降级为仅标题闪烁。
        try {
          const req = N.requestPermission?.();
          if (req && typeof (req as Promise<unknown>).then === 'function') {
            (req as Promise<Permission>).then(setPermission).catch(() => {});
          }
        } catch {
          /* 请求被拒/环境不支持：保持现状 */
        }
      }
    }
  }, []);

  const setActiveSessionId = useCallback((id: string | null) => {
    activeSessionIdRef.current = id;
  }, []);

  // ── 通知处理（稳定回调，读 ref 取最新值）──────────────────
  const handleNotification = useCallback(
    (n: AgentNotification) => {
      const title = (() => {
        switch (n.event) {
          case 'turn_done':
            return tRef.current('agent.notifTurnDone');
          case 'turn_error':
            return tRef.current('agent.notifTurnError');
          case 'approval_needed':
            return tRef.current('agent.notifApproval');
          case 'elicitation_needed':
            return tRef.current('agent.notifElicitation');
        }
      })();
      const body = (() => {
        switch (n.event) {
          case 'turn_done':
            return tRef.current('agent.notifTurnDoneBody');
          case 'turn_error':
            return n.message || tRef.current('agent.notifTurnErrorBody');
          case 'approval_needed':
            return n.tool
              ? tRef.current('agent.notifApprovalBody', { tool: n.tool })
              : n.summary;
          case 'elicitation_needed':
            return n.message || tRef.current('agent.notifElicitationBody');
        }
      })();
      flashTitle(title);

      const N = getNotificationApi();
      if (permissionRef.current === 'granted' && N) {
        try {
          const notif = new N(title, {
            body,
            tag: `${NOTIFICATION_TAG_PREFIX}${n.event}-${n.session_id}`,
          });
          notif.onclick = () => {
            window.focus();
            stopFlashing();
            // 记录选中会话，让 /agent 打开对应会话
            try {
              localStorage.setItem('agent.lastSessionId', n.session_id);
            } catch {
              /* ignore */
            }
            navigate('/agent');
          };
        } catch {
          /* 某些环境构造失败则忽略（降级为仅标题闪烁） */
        }
      }
    },
    [navigate, flashTitle, stopFlashing],
  );

  // ── 全局通知 WS：enabled 时建立，断线指数退避重连 ──────────
  useEffect(() => {
    if (!enabled) return;
    let ws: WebSocket | null = null;
    let closedByCleanup = false;
    let attempts = 0;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;

    const connect = () => {
      ws = new WebSocket(agentNotificationsWsUrl());
      ws.onmessage = (ev) => {
        let n: AgentNotification;
        try {
          n = JSON.parse(ev.data) as AgentNotification;
        } catch {
          return;
        }
        if (
          !shouldNotify(n, {
            enabled: enabledRef.current,
            activeSessionId: activeSessionIdRef.current,
            tabVisible: typeof document !== 'undefined' && !document.hidden,
          })
        ) {
          return;
        }
        handleNotification(n);
      };
      ws.onclose = () => {
        if (closedByCleanup) return;
        const delay = Math.min(1000 * 2 ** attempts, MAX_RECONNECT_MS);
        attempts++;
        reconnectTimer = setTimeout(connect, delay);
      };
      ws.onerror = () => {
        // onerror 之后浏览器必发 onclose，统一在那里重连
      };
    };

    connect();
    return () => {
      closedByCleanup = true;
      if (reconnectTimer) {
        clearTimeout(reconnectTimer);
        reconnectTimer = null;
      }
      if (ws) {
        ws.onclose = null;
        ws.onerror = null;
        ws.onmessage = null;
        ws.close();
      }
    };
  }, [enabled, handleNotification]);

  return (
    <AgentNotificationsContext.Provider
      value={{ enabled, permission, setEnabled, setActiveSessionId }}
    >
      {children}
    </AgentNotificationsContext.Provider>
  );
}

export function useAgentNotifications(): AgentNotificationsContextValue {
  const ctx = useContext(AgentNotificationsContext);
  if (!ctx) {
    throw new Error('useAgentNotifications must be used within AgentNotificationsProvider');
  }
  return ctx;
}

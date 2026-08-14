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
/** 连接假死判定阈值：服务端应用层心跳每 25s 一帧，连续 3 个心跳周期（75s）
 *  无任何帧即认为连接被中间设备静默掐断（半开 TCP 不触发 onclose），由看门狗
 *  主动 close 走既有 onclose 指数退避重连。 */
const HEARTBEAT_TIMEOUT_MS = 75_000;
/** 看门狗扫描周期：远小于心跳超时，保证假死判定延迟在可接受范围。 */
const WATCHDOG_INTERVAL_MS = 30_000;

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
  // 最近一帧到达时间（含应用层心跳）：看门狗据此判定连接假死（半开 TCP 不触发
  // onclose，长任务静默期间浏览器不会自行断开——没有探活就永远发现不了）。
  // 组件级 ref：enabled 开关翻转重建 effect 时保留基线。
  const lastFrameAtRef = useRef(0);

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
    // 连接假死看门狗：中间设备静默掐断 TCP 时 onclose 不触发，浏览器重连逻辑
    // 依赖 onclose 永远不会执行。每 WATCHDOG_INTERVAL_MS 检查最近一帧，超过
    // HEARTBEAT_TIMEOUT_MS 无帧即主动 close——走既有 onclose 指数退避重连。
    let watchdogTimer: ReturnType<typeof setInterval> | null = null;

    const connect = () => {
      ws = new WebSocket(agentNotificationsWsUrl());
      ws.onopen = () => {
        // 新连接给足一个完整心跳窗口：onopen 即重置看门狗基线（此后每帧刷新）
        lastFrameAtRef.current = Date.now();
      };
      ws.onmessage = (ev) => {
        // 任意帧（含应用层心跳）到达都刷新看门狗基线：连接活着即不被误判假死
        lastFrameAtRef.current = Date.now();
        let n: AgentNotification;
        try {
          n = JSON.parse(ev.data) as AgentNotification;
        } catch {
          return;
        }
        // 应用层心跳帧（服务端每 25s 一帧）：仅探活，不触发任何通知。
        // 心跳无 session_id，shouldNotify 在「前台 + 无活跃会话」时会对它返回 true
        // 并误弹一条空通知，必须在此显式滤掉。
        if ((n as { type?: string }).type === 'heartbeat') return;
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
    watchdogTimer = globalThis.setInterval(() => {
      const w = ws;
      if (!w || w.readyState !== WebSocket.OPEN) return;
      if (Date.now() - lastFrameAtRef.current > HEARTBEAT_TIMEOUT_MS) {
        // 连接假死（半开 TCP 不触发 onclose）：主动 close 走既有重连路径
        w.close();
      }
    }, WATCHDOG_INTERVAL_MS);
    return () => {
      closedByCleanup = true;
      if (watchdogTimer) {
        clearInterval(watchdogTimer);
        watchdogTimer = null;
      }
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

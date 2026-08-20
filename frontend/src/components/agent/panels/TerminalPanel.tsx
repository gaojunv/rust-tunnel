import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';
import { Button } from '@/components/ui/button';
import { RotateCcw } from 'lucide-react';
import { agentTerminalWsUrl } from '../../../api/client';

type ConnState = 'connecting' | 'connected' | 'disconnected' | 'error';

const DOT_CLASS: Record<ConnState, string> = {
  connecting: 'bg-yellow-400',
  connected: 'bg-emerald-500',
  disconnected: 'bg-slate-400',
  error: 'bg-slate-400',
};

const STATUS_KEY = {
  connecting: 'agent.terminalConnecting',
  connected: 'agent.terminalConnected',
  disconnected: 'agent.terminalDisconnected',
  error: 'agent.terminalError',
} as const;

export default function TerminalPanel({ workspaceId }: { workspaceId: string }) {
  const { t } = useTranslation();
  const containerRef = useRef<HTMLDivElement>(null);
  const [status, setStatus] = useState<ConnState>('connecting');
  // reconnectNonce +1 → effect 重跑，完整重建 xterm 与 WebSocket
  const [reconnectNonce, setReconnectNonce] = useState(0);

  useEffect(() => {
    if (!workspaceId) return;

    // 初始化时跟随当前明暗主题（主题切换不重建终端——MVP 接受）
    const dark = document.documentElement.classList.contains('dark');
    const term = new Terminal({
      fontSize: 12,
      fontFamily: 'ui-monospace, monospace',
      cursorBlink: true,
      theme: dark
        ? { background: '#0f172a', foreground: '#e2e8f0' }
        : { background: '#ffffff', foreground: '#1e293b' },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(containerRef.current!);
    fit.fit();

    setStatus('connecting');
    // 二进制帧双向透传 PTY 字节流；首次协商以当前终端尺寸建立会话
    const ws = new WebSocket(agentTerminalWsUrl(workspaceId, term.cols, term.rows));
    ws.binaryType = 'arraybuffer';

    const dataSub = term.onData((d) => {
      if (ws.readyState === WebSocket.OPEN) {
        // 必须编码为字节发送 Binary 帧：`ws.send(string)` 会发 Text 帧，
        // 后端 bridge_terminal 只消费 Message::Binary，Text 帧被静默丢弃 → 按键全丢。
        ws.send(new TextEncoder().encode(d));
      }
    });

    ws.onopen = () => {
      setStatus('connected');
      term.focus();
    };
    ws.onmessage = (ev: MessageEvent) => {
      if (typeof ev.data === 'string') {
        // 后端异常（如老客户端不支持 PTY）以文本帧返回错误信息
        term.write(`\r\n\x1b[31m${ev.data}\x1b[0m\r\n`);
      } else {
        term.write(new Uint8Array(ev.data as ArrayBuffer));
      }
    };
    ws.onclose = () => {
      setStatus('disconnected');
      term.write('\r\n\x1b[90m[connection closed]\x1b[0m\r\n');
    };
    ws.onerror = () => {
      setStatus('error');
    };

    // 容器尺寸变化 → 重新 fit + 防抖上报 PTY resize（~200ms 避免拖拽期间刷屏）
    let ro: ResizeObserver | null = null;
    let resizeTimer: ReturnType<typeof setTimeout> | null = null;
    if (typeof ResizeObserver !== 'undefined') {
      ro = new ResizeObserver(() => {
        try {
          fit.fit();
        } catch {
          // 面板隐藏/容器无尺寸时忽略
        }
        // 防抖上报 PTY 尺寸：仅 WS OPEN 时发送
        if (resizeTimer) clearTimeout(resizeTimer);
        resizeTimer = setTimeout(() => {
          if (ws.readyState === WebSocket.OPEN) {
            try {
              ws.send(JSON.stringify({ type: 'resize', cols: term.cols, rows: term.rows }));
            } catch { /* ws closed between check and send */ }
          }
        }, 200);
      });
      ro.observe(containerRef.current!);
    }

    return () => {
      ro?.disconnect();
      if (resizeTimer) clearTimeout(resizeTimer);
      dataSub.dispose();
      ws.onopen = null;
      ws.onmessage = null;
      ws.onclose = null;
      ws.onerror = null;
      ws.close();
      term.dispose();
    };
  }, [workspaceId, reconnectNonce]);

  // 空态：无 workspaceId（正常使用时 ActivityBar 保证有值）——防御性占位
  if (!workspaceId) {
    return (
      <div className="flex h-full flex-col">
        <div className="flex items-center gap-2 border-b border-border/60 px-2 py-1.5 text-xs font-medium">
          <span className="h-2 w-2 rounded-full bg-slate-400" />
          {t('agent.terminal')}
        </div>
        <div className="flex min-h-0 flex-1 items-center justify-center p-4 text-xs text-muted-foreground">
          {t('agent.terminalDisconnected')}
        </div>
      </div>
    );
  }

  const showReconnect = status === 'disconnected' || status === 'error';

  return (
    <div className="flex h-full flex-col">
      <div className="flex items-center justify-between gap-2 border-b border-border/60 px-2 py-1.5">
        <div className="flex min-w-0 items-center gap-2 text-xs font-medium">
          <span className={`h-2 w-2 shrink-0 rounded-full ${DOT_CLASS[status]}`} />
          <span className="truncate">{t('agent.terminal')}</span>
          <span className="text-muted-foreground">{t(STATUS_KEY[status])}</span>
        </div>
        {showReconnect && (
          <Button
            variant="ghost"
            size="sm"
            onClick={() => setReconnectNonce((n) => n + 1)}
            aria-label={t('agent.terminalReconnect')}
            className="h-7 shrink-0 gap-1 px-2 text-xs"
          >
            <RotateCcw className="h-3.5 w-3.5" />
            {t('agent.terminalReconnect')}
          </Button>
        )}
      </div>
      <div ref={containerRef} className="min-h-0 flex-1" />
    </div>
  );
}

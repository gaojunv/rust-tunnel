import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';
import { Button } from '@/components/ui/button';
import { Copy, ClipboardPaste, RotateCcw, Trash2 } from 'lucide-react';
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

// 明/暗两套完整主题：cursor/selection 颜色必须显式给出。
// 浅色缺省 cursor 为 #ffffff → 白底不可见（回归根源）；selectionBackground
// 缺省半透明白，白底同样看不清。深色沿用既有的 slate 配色。
export const TERMINAL_THEME = {
  light: {
    background: '#ffffff',
    foreground: '#1e293b',
    cursor: '#1e293b',
    cursorAccent: '#ffffff',
    selectionBackground: 'rgba(59,130,246,0.30)',
    selectionInactiveBackground: 'rgba(59,130,246,0.20)',
  },
  dark: {
    background: '#0f172a',
    foreground: '#e2e8f0',
    cursor: '#e2e8f0',
    cursorAccent: '#0f172a',
    selectionBackground: 'rgba(96,165,250,0.35)',
    selectionInactiveBackground: 'rgba(96,165,250,0.25)',
  },
} as const;

const isDarkTheme = () => document.documentElement.classList.contains('dark');

// 快捷键处理抽成纯函数便于单测：
// - Ctrl/Cmd+Shift+C → 复制，阻止透传
// - Ctrl/Cmd+Shift+V → 粘贴，阻止透传
// - Ctrl+C 有选区 → 复制并阻止透传；无选区 → 放行 SIGINT（VS Code 行为）
export function handleTerminalKey(
  e: KeyboardEvent,
  actions: { copy: () => void; paste: () => void; hasSelection: () => boolean },
): boolean {
  const mod = e.ctrlKey || e.metaKey;
  const key = e.key.toLowerCase();
  if (!mod || !e.shiftKey) {
    if (key === 'c' && mod && actions.hasSelection()) {
      actions.copy();
      return false;
    }
    return true;
  }
  if (key === 'c') {
    actions.copy();
    return false;
  }
  if (key === 'v') {
    actions.paste();
    return false;
  }
  return true;
}

export default function TerminalPanel({ workspaceId }: { workspaceId: string }) {
  const { t } = useTranslation();
  const containerRef = useRef<HTMLDivElement>(null);
  const [status, setStatus] = useState<ConnState>('connecting');
  // 按钮操作后短暂显示“已复制”态（2s）
  const [copied, setCopied] = useState(false);
  const copiedTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  // reconnectNonce +1 → effect 重跑，完整重建 xterm 与 WebSocket
  const [reconnectNonce, setReconnectNonce] = useState(0);
  // 复制/清屏按钮在断开态也可用，故 term 用 ref 跨 effect 暴露
  const termRef = useRef<Terminal | null>(null);
  const wsRef = useRef<WebSocket | null>(null);

  // 复制：优先异步 Clipboard API，失败降级为隐藏 textarea + execCommand（移动端兼容）
  const copyTerminalText = async (term: Terminal) => {
    let text = '';
    if (term.hasSelection()) {
      text = term.getSelection();
    } else {
      // 无选区 → 取整个滚动缓冲（buffer.active 是视口内的滚动缓冲，含回滚行）
      const lines: string[] = [];
      for (let i = 0; i < term.buffer.active.length; i++) {
        lines.push(term.buffer.active.getLine(i)!.translateToString(true));
      }
      // 去尾部空行，保留中间空行
      while (lines.length > 0 && lines[lines.length - 1].trim() === '') lines.pop();
      text = lines.join('\n');
    }
    if (!text) return;
    try {
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(text);
        return;
      }
      throw new Error('clipboard api unavailable');
    } catch {
      // 降级：隐藏 textarea + execCommand('copy')
      const ta = document.createElement('textarea');
      ta.value = text;
      ta.style.position = 'fixed';
      ta.style.opacity = '0';
      document.body.appendChild(ta);
      ta.select();
      try {
        document.execCommand('copy');
      } finally {
        document.body.removeChild(ta);
      }
    }
  };

  const showCopied = () => {
    setCopied(true);
    if (copiedTimer.current) clearTimeout(copiedTimer.current);
    copiedTimer.current = setTimeout(() => setCopied(false), 2000);
  };

  const doCopy = async () => {
    const term = termRef.current;
    if (!term) return;
    await copyTerminalText(term);
    showCopied();
  };
  const doCopyRef = useRef(doCopy);
  doCopyRef.current = doCopy;

  const doPaste = async () => {
    const ws = wsRef.current;
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    try {
      const text = await navigator.clipboard.readText();
      if (text) {
        // 必须编码为字节发送 Binary 帧：`ws.send(string)` 会发 Text 帧，
        // 后端 bridge_terminal 只消费 Message::Binary，Text 帧被静默丢弃。
        ws.send(new TextEncoder().encode(text));
      }
    } catch {
      // 用户拒绝授权/浏览器不支持 readText → 忽略
    }
  };
  const doPasteRef = useRef(doPaste);
  doPasteRef.current = doPaste;

  const doClear = () => {
    termRef.current?.clear();
  };

  useEffect(() => {
    if (!workspaceId) return;

    // 初始化时跟随当前明暗主题
    const term = new Terminal({
      fontSize: 12,
      fontFamily: 'ui-monospace, monospace',
      cursorBlink: true,
      rightClickSelectsWord: true,
      theme: isDarkTheme() ? TERMINAL_THEME.dark : TERMINAL_THEME.light,
    });
    termRef.current = term;
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(containerRef.current!);
    fit.fit();

    // 主题切换热更新：监听 <html> 的 class 变化，暗 ⇄ 明时实时换肤。
    // 修的是「MVP 接受」那条：此前注释「主题切换不重建终端——MVP 接受」。
    const themeObserver = new MutationObserver(() => {
      term.options.theme = isDarkTheme() ? TERMINAL_THEME.dark : TERMINAL_THEME.light;
    });
    themeObserver.observe(document.documentElement, { attributes: true, attributeFilter: ['class'] });

    // 桌面快捷键：Ctrl/Cmd+Shift+C/V 复制/粘贴，Ctrl+C 有选区复制、无选区透传 SIGINT
    term.attachCustomKeyEventHandler((e) =>
      handleTerminalKey(e, {
        copy: () => { void doCopyRef.current(); },
        paste: () => { void doPasteRef.current(); },
        hasSelection: () => term.hasSelection(),
      }),
    );

    setStatus('connecting');
    // 二进制帧双向透传 PTY 字节流；首次协商以当前终端尺寸建立会话
    const ws = new WebSocket(agentTerminalWsUrl(workspaceId, term.cols, term.rows));
    wsRef.current = ws;
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
      themeObserver.disconnect();
      dataSub.dispose();
      ws.onopen = null;
      ws.onmessage = null;
      ws.onclose = null;
      ws.onerror = null;
      ws.close();
      term.dispose();
      if (termRef.current === term) termRef.current = null;
      if (wsRef.current === ws) wsRef.current = null;
    };
  }, [workspaceId, reconnectNonce]);

  // 卸载时清掉复制态计时器
  useEffect(() => () => {
    if (copiedTimer.current) clearTimeout(copiedTimer.current);
  }, []);

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
  // 复制/清屏在断开态也可用；粘贴需要活动连接
  const wsOpen = wsRef.current?.readyState === WebSocket.OPEN;
  // 新 key 尚未写入 i18n JSON（避免并发冲突），本地转宽类型以过 tsc 严格校验
  const tc = t as unknown as (k: string) => string;

  return (
    <div className="flex h-full flex-col">
      <div className="flex items-center justify-between gap-2 border-b border-border/60 px-2 py-1.5">
        <div className="flex min-w-0 items-center gap-2 text-xs font-medium">
          <span className={`h-2 w-2 shrink-0 rounded-full ${DOT_CLASS[status]}`} />
          <span className="truncate">{t('agent.terminal')}</span>
          <span className="text-muted-foreground">{t(STATUS_KEY[status])}</span>
        </div>
        <div className="flex shrink-0 items-center gap-0.5">
          <Button
            variant="ghost"
            size="sm"
            onClick={() => {
              void doCopy();
            }}
            aria-label={tc('agent.terminalCopy')}
            className="h-7 shrink-0 px-2 text-xs"
          >
            <Copy className="h-3.5 w-3.5" />
            {copied && <span className="text-emerald-500">{tc('agent.terminalCopied')}</span>}
          </Button>
          <Button
            variant="ghost"
            size="sm"
            disabled={!wsOpen}
            onClick={() => {
              void doPaste();
            }}
            aria-label={tc('agent.terminalPaste')}
            className="h-7 shrink-0 px-2 text-xs"
          >
            <ClipboardPaste className="h-3.5 w-3.5" />
          </Button>
          <Button
            variant="ghost"
            size="sm"
            onClick={doClear}
            aria-label={tc('agent.terminalClear')}
            className="h-7 shrink-0 px-2 text-xs"
          >
            <Trash2 className="h-3.5 w-3.5" />
          </Button>
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
      </div>
      <div ref={containerRef} className="min-h-0 flex-1" />
    </div>
  );
}

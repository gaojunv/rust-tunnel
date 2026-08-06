// @vitest-environment jsdom
import { describe, expect, it, vi, beforeEach, afterEach, type Mock } from 'vitest';
import { cleanup, render, screen, act, fireEvent } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { listAgentMessages, listWorkspaceFiles } from '../../api/client';
import ChatStream from './ChatStream';

vi.mock('react-i18next', () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

vi.mock('../../api/client', () => ({
  listAgentMessages: vi.fn().mockResolvedValue([]),
  updateAgentSessionModel: vi.fn().mockResolvedValue(undefined),
  getAgentDefaultModel: vi.fn().mockResolvedValue(''),
  listWorkspaceFiles: vi.fn().mockResolvedValue({ files: [] }),
  agentWsUrl: () => 'ws://test/ws',
}));

vi.mock('../../api/agentModels', () => ({
  listAgentSelectableModels: vi.fn().mockResolvedValue({ models: [], groups: [] }),
}));

// 捕获 ws 实例以便手动触发 onmessage
const wsInstances: FakeWs[] = [];
let wsInstance: FakeWs | null = null;
class FakeWs {
  static OPEN = 1;
  readyState = 1;
  sent: string[] = [];
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onopen: (() => void) | null = null;
  constructor() {
    // eslint-disable-next-line @typescript-eslint/no-this-alias -- 捕获实例以便手动触发 onmessage
    wsInstance = this;
    wsInstances.push(this);
  }
  send(s: string) {
    this.sent.push(s);
  }
  close() {}
  emit(msg: object) {
    this.onmessage?.({ data: JSON.stringify(msg) });
  }
}

const renderChat = () => {
  const qc = new QueryClient({
    defaultOptions: {
      // refetchOnMount:false — ChatStream 的 history effect 依赖「挂载时只装载一次」
      // （done/重连后显式 invalidate 才会重新装载）。默认 refetchOnMount 会让 WS
      // effect 触发的无关 state 更新也引发 refetch → 覆盖聊天区实时增量。
      queries: { retry: false, refetchOnMount: false },
    },
  });
  return render(
    <QueryClientProvider client={qc}>
      <ChatStream sessionId="s1" workspaceId="w1" model="" onModelChange={vi.fn()} />
    </QueryClientProvider>
  );
};

describe('ChatStream running state', () => {
  beforeEach(() => {
    vi.stubGlobal('WebSocket', FakeWs as unknown as typeof WebSocket);
    wsInstance = null;
    wsInstances.length = 0;
  });
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it('stays running after tool_call, clears on tool_result + done', async () => {
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    expect(screen.getByText('agent.running')).toBeTruthy();
    act(() => {
      wsInstance!.emit({ type: 'tool_result', id: 'c1', name: 'list_dir', result: 'ok' });
    });
    // tool 回齐但未收到 done → 仍 Running
    expect(screen.getByText('agent.running')).toBeTruthy();
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '完成' });
      wsInstance!.emit({ type: 'done' });
    });
    expect(screen.queryByText('agent.running')).toBeNull();
  });

  it('clears running on error even with pending tools', async () => {
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    act(() => {
      wsInstance!.emit({ type: 'error', message: 'boom' });
    });
    expect(screen.queryByText('agent.running')).toBeNull();
  });

  it('clears running on done even with lost tool_result frames', async () => {
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    // tool_result 帧丢失（断线场景），done 到达即应解除 running
    act(() => {
      wsInstance!.emit({ type: 'done' });
    });
    expect(screen.queryByText('agent.running')).toBeNull();
  });

  it('reconnects after close and shows reconnecting banner', async () => {
    // 重连退避首次 1s → 本测试内压缩到 1ms 以同步触发（spy 随 afterEach 恢复）
    const origSetTimeout = globalThis.setTimeout;
    vi.spyOn(globalThis, 'setTimeout').mockImplementation(((cb: () => void, ms?: number) => {
      return origSetTimeout(cb, ms !== undefined && ms >= 1000 && ms <= 15000 ? 1 : ms) as ReturnType<typeof setTimeout>;
    }) as typeof setTimeout);
    renderChat();
    expect(wsInstances).toHaveLength(1);
    act(() => {
      wsInstances[0].onclose?.();
    });
    // 断线横幅出现
    expect(screen.getByText('agent.reconnecting')).toBeTruthy();
    // 退避（测试内 1ms）后自动重连
    await act(async () => {
      await new Promise((r) => origSetTimeout(r, 20));
    });
    expect(wsInstances.length).toBeGreaterThan(1);
    act(() => {
      wsInstances[wsInstances.length - 1].onopen?.();
    });
    expect(screen.queryByText('agent.reconnecting')).toBeNull();
  });

  it('warns about possibly-lost message when closed mid-run', async () => {
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    expect(screen.getByText('agent.running')).toBeTruthy();
    // tool_call 引发状态更新 → React Query 对 stale 查询后台 refetch → 挂载过
    // 的 history effect 重新执行（loadedRef 已 true，直接跳过），但 WS effect
    // 不会因此重建——取「当前活跃连接」（wsInstance）触发关闭。
    act(() => {
      wsInstance!.onclose?.();
    });
    // running 解除 + 中断提示（刚发的消息可能未处理）
    expect(screen.queryByText('agent.running')).toBeNull();
    expect(screen.getByText(/agent.connectionInterrupted/)).toBeTruthy();
  });

  it('force-clears running after 10min timeout', async () => {
    // 说明：vitest v4 + jsdom 环境下，组件模块内部调用的 setTimeout 不会被
    // vi.advanceTimersByTime 驱动（见 task-9 报告），因此改为 spy 捕获 10 分钟
    // 超时回调并确定性触发；断言语义不变：超时兜底必须无条件解除 Running。
    let timeoutCb: (() => void) | undefined;
    const origSetTimeout = globalThis.setTimeout;
    vi.spyOn(globalThis, 'setTimeout').mockImplementation(
      ((cb: () => void, ms?: number) => {
        if (ms === 10 * 60 * 1000) timeoutCb = cb;
        return origSetTimeout(cb, ms ?? 0) as ReturnType<typeof setTimeout>;
      }) as typeof setTimeout,
    );
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    expect(screen.getByText('agent.running')).toBeTruthy();
    act(() => {
      timeoutCb?.();
    });
    expect(screen.queryByText('agent.running')).toBeNull();
  });

  it('renders new-format tool_calls/tool_result history', async () => {
    (listAgentMessages as Mock).mockResolvedValue([
      { id: 'm1', session_id: 's1', role: 'user', content: '看下文件', tool_calls: null, tool_call_id: null, name: null, kind: 'message', created_at: '2026-08-05' },
      { id: 'm2', session_id: 's1', role: 'assistant', content: '', tool_calls: JSON.stringify([{ id: 'c1', type: 'function', function: { name: 'read_file', arguments: '{"path":"a.rs"}' } }]), tool_call_id: null, name: null, kind: 'tool_calls', created_at: '2026-08-05' },
      { id: 'm3', session_id: 's1', role: 'tool', content: 'fn main(){}', tool_calls: null, tool_call_id: 'c1', name: 'read_file', kind: 'tool_result', created_at: '2026-08-05' },
      { id: 'm4', session_id: 's1', role: 'assistant', content: '文件里是 main 函数', tool_calls: null, tool_call_id: null, name: null, kind: 'message', created_at: '2026-08-05' },
    ]);
    renderChat();
    // 工具名、参数、结果都渲染出来（工具卡片默认收起，先点头部展开再断言 args/result）
    expect(await screen.findByText('read_file')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { expanded: false }));
    expect(screen.getByText(/fn main\(\)/)).toBeTruthy();
    expect(screen.getByText('文件里是 main 函数')).toBeTruthy();
  });

  it('renders legacy-format tool row with kind=message role=tool', async () => {
    // 迁移前遗留行：SQLite ALTER TABLE DEFAULT 补 role='tool' 但 kind='message'
    (listAgentMessages as Mock).mockResolvedValue([
      { id: 'm1', session_id: 's1', role: 'tool', content: '', tool_calls: JSON.stringify([{ name: 'shell', args: '{"cmd":"ls"}', result: 'a.rs' }]), tool_call_id: null, name: null, kind: 'message', created_at: '2026-08-05' },
    ]);
    renderChat();
    expect(await screen.findByText('shell')).toBeTruthy();
    // 工具卡片默认收起，先展开再断言结果
    fireEvent.click(screen.getByRole('button', { expanded: false }));
    expect(screen.getByText('a.rs')).toBeTruthy();
  });

  it('merges streamed assistant_chunk deltas into one bubble', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '你好', final: false });
      wsInstance!.emit({ type: 'assistant_chunk', content: '，世界', final: false });
      wsInstance!.emit({ type: 'assistant_chunk', content: '', final: true });
    });
    // 一个气泡，内容为拼接结果
    const bubbles = screen.getAllByText('你好，世界');
    expect(bubbles).toHaveLength(1);
  });

  it('renders status event as transient hint', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'status', message: 'compacting' });
    });
    expect(await screen.findByText(/compacting|压缩/)).toBeTruthy();
  });

  it('renders non-SSE fallback (content + final in one chunk) as a single bubble', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '非流式回退完整文本', final: true });
    });
    // 一条 content+final:true 消息：先追加内容再关闭气泡 → 单个完整气泡
    const bubbles = screen.getAllByText('非流式回退完整文本');
    expect(bubbles).toHaveLength(1);
  });

  it('status closes the current streaming bubble before appending the hint', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'assistant_chunk', content: '流式', final: false });
      wsInstance!.emit({ type: 'status', message: 'compacting' });
      wsInstance!.emit({ type: 'assistant_chunk', content: '后续', final: false });
      wsInstance!.emit({ type: 'assistant_chunk', content: '', final: true });
    });
    // status 断开流式气泡：'流式' 与 '后续' 各自独立，不合并
    expect(screen.getByText('流式')).toBeTruthy();
    expect(screen.getByText('后续')).toBeTruthy();
    expect(screen.queryByText('流式后续')).toBeNull();
  });

  it('dedups re-inserted kept segment after compaction (M3)', async () => {
    // DB 物理顺序：[旧消息..., 原kept..., summary, 重插kept...]——压缩修复
    // （801c9a6）使 kept 段以相同内容出现两次，前端必须只渲染一份。
    // K = summary 后行数（含 tool_calls/tool_result 行），跳过 summary 前最后 K 行。
    const row = (id: string, role: string, content: string, kind: string) => ({
      id,
      session_id: 's1',
      role,
      content,
      tool_calls: null,
      tool_call_id: null,
      name: null,
      kind,
      created_at: '2026-08-05',
    });
    const toolCalls = JSON.stringify([
      { id: 'c1', type: 'function', function: { name: 'read_file', arguments: '{"path":"a.rs"}' } },
    ]);
    const toolCallsRow = (id: string) => ({
      ...row(id, 'assistant', '', 'tool_calls'),
      tool_calls: toolCalls,
    });
    const toolResultRow = (id: string) => ({
      ...row(id, 'tool', 'fn main(){}', 'tool_result'),
      tool_call_id: 'c1',
      name: 'read_file',
    });
    (listAgentMessages as Mock).mockResolvedValue([
      row('old1', 'user', '早期问题', 'message'),
      row('old2', 'assistant', '早期回答', 'message'),
      // 原 kept 段（summary 前，含 tool 配对行）——应被跳过
      row('k1', 'user', '保留问题', 'message'),
      toolCallsRow('k2'),
      toolResultRow('k3'),
      row('sum', 'user', '[上下文摘要] 之前讨论了 A', 'summary'),
      // 重插 kept 段（summary 后）——只渲染这一份
      row('k1r', 'user', '保留问题', 'message'),
      toolCallsRow('k2r'),
      toolResultRow('k3r'),
    ]);
    renderChat();
    // 旧消息与 summary 完整保留
    expect(await screen.findByText('早期问题')).toBeTruthy();
    expect(screen.getByText('早期回答')).toBeTruthy();
    expect(screen.getByText('[上下文摘要] 之前讨论了 A')).toBeTruthy();
    // 重插的 kept 段只渲染一次（原始 kept 行被跳过，无连续重复段）
    expect(screen.getAllByText('保留问题')).toHaveLength(1);
    // 工具卡片同样只渲染一份（read_file 工具名 + 结果；默认收起，先展开再断言结果）
    expect(screen.getAllByText('read_file')).toHaveLength(1);
    fireEvent.click(screen.getByRole('button', { expanded: false }));
    expect(screen.getAllByText(/fn main\(\)/)).toHaveLength(1);
  });

  it('keeps legit history when new messages follow compaction (over-skip fix)', async () => {
    // 压缩后用户继续对话：DB 顺序 [..., 原kept, summary, 重插kept, 新消息...]。
    // 旧去重逻辑把「summary 后行数」当作重插行数，会多跳掉 summary 前没有重复
    // 副本的合法旧行；内容匹配去重应只跳过真正的重复原件。
    const row = (id: string, role: string, content: string, kind: string) => ({
      id,
      session_id: 's1',
      role,
      content,
      tool_calls: null,
      tool_call_id: null,
      name: null,
      kind,
      created_at: '2026-08-05',
    });
    (listAgentMessages as Mock).mockResolvedValue([
      row('old1', 'user', '最早的问题', 'message'),
      row('old2', 'assistant', '最早的回答', 'message'),
      row('k1', 'user', '保留问题', 'message'),
      row('k2', 'assistant', '保留回答', 'message'),
      row('sum', 'user', '[上下文摘要] 之前讨论了 A', 'summary'),
      row('k1r', 'user', '保留问题', 'message'),
      row('k2r', 'assistant', '保留回答', 'message'),
      // 压缩之后的新对话（排在 summary 后，但不是重插副本）
      row('new1', 'user', '压缩后的新问题', 'message'),
      row('new2', 'assistant', '压缩后的新回答', 'message'),
    ]);
    renderChat();
    // 合法旧消息不能被多跳掉
    expect(await screen.findByText('最早的问题')).toBeTruthy();
    expect(screen.getByText('最早的回答')).toBeTruthy();
    // 重插 kept 只渲染一份
    expect(screen.getAllByText('保留问题')).toHaveLength(1);
    expect(screen.getAllByText('保留回答')).toHaveLength(1);
    // 压缩后的新消息正常渲染
    expect(screen.getByText('压缩后的新问题')).toBeTruthy();
    expect(screen.getByText('压缩后的新回答')).toBeTruthy();
  });

  it('running 时显示停止按钮，点击发送 cancel 并解除 running', async () => {
    renderChat();
    // 进入 running：与既有 running 测试一致，用 tool_call 帧驱动 armRunning
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    expect(screen.getByText('agent.running')).toBeTruthy();
    // 停止按钮（aria-label = t('agent.stop')）替代发送按钮
    const stopBtn = screen.getByRole('button', { name: 'agent.stop' });
    expect(stopBtn).toBeTruthy();
    // 捕获当前活跃连接：i18n mock 的 t 每次渲染返回新引用 → armRunning（WS effect
    // 依赖）不稳定 → 每次 state 更新都重建 WebSocket（实例轮换），点击后 wsInstance
    // 已指向更新的实例；在点击前捕获当前实例再断言其 send。
    const ws = wsInstance!;
    act(() => {
      stopBtn.click();
    });
    // mockWs.send 被调用且 payload 含 '"type":"cancel"'
    expect(ws.sent.some((s) => s.includes('"type":"cancel"'))).toBe(true);
    // running 指示消失 + 停止提示气泡出现
    expect(screen.queryByText('agent.running')).toBeNull();
    expect(screen.getByText(/agent.stopped/)).toBeTruthy();
  });

  it('收到 stopped 帧解除 running', async () => {
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'list_dir', args: '{}' });
    });
    expect(screen.getByText('agent.running')).toBeTruthy();
    // 服务端确认取消（本连接或其他标签页的 cancel 都经 WS 广播 stopped）
    act(() => {
      wsInstance!.emit({ type: 'stopped' });
    });
    expect(screen.queryByText('agent.running')).toBeNull();
  });

  it('invalidates agent-sessions cache on session_title frame', async () => {
    // 标题生成晚于 done 帧（需数秒），done 时 refetch 早于标题写库——服务端
    // 写库后另发 session_title 帧，前端据此刷新会话列表让 SessionBar 回显。
    // 与 done 帧的 invalidate 同一判定方式：spy QueryClient.invalidateQueries。
    (listAgentMessages as Mock).mockResolvedValue([]);
    const qc = new QueryClient({
      defaultOptions: { queries: { retry: false, refetchOnMount: false } },
    });
    const invalidateSpy = vi.spyOn(qc, 'invalidateQueries');
    render(
      <QueryClientProvider client={qc}>
        <ChatStream sessionId="s1" workspaceId="w1" model="" onModelChange={vi.fn()} />
      </QueryClientProvider>
    );
    act(() => {
      wsInstance!.emit({ type: 'session_title', title: '修复登录 bug', session_id: 's1' });
    });
    expect(invalidateSpy).toHaveBeenCalledWith({ queryKey: ['agent-sessions'] });
  });

  it('renders summary rows as assistant bubbles (M5)', async () => {
    const row = (id: string, role: string, content: string, kind: string) => ({
      id,
      session_id: 's1',
      role,
      content,
      tool_calls: null,
      tool_call_id: null,
      name: null,
      kind,
      created_at: '2026-08-05',
    });
    (listAgentMessages as Mock).mockResolvedValue([
      row('u', 'user', '普通用户消息', 'message'),
      row('s', 'user', '[上下文摘要] 之前讨论了 X', 'summary'),
    ]);
    renderChat();
    const userEl = await screen.findByText('普通用户消息');
    const summaryEl = screen.getByText('[上下文摘要] 之前讨论了 X');
    // summary 走 assistant 气泡样式（mr-auto + bg-muted），而非用户气泡（ml-auto）
    const userBubble = userEl.closest('[class*="ml-auto"]');
    const summaryBubble = summaryEl.closest('[class*="mr-auto"]');
    expect(userBubble?.className).toContain('bg-primary/10');
    expect(summaryBubble?.className).toContain('bg-muted');
  });

  it('renders approval card and responds on approve', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    // 注入 approval_request 帧：卡片应出现（标题 + 工具名 + 摘要）
    act(() => {
      wsInstance!.emit({
        type: 'approval_request',
        request_id: 'req1',
        tool: 'shell',
        summary: 'rm -rf /tmp/x',
        args_preview: '{"cmd":"rm -rf /tmp/x"}',
      });
    });
    // 标题文案后紧跟冒号与工具名（跨元素），用子串匹配
    expect(screen.getByText(/agent\.approvalRequired/)).toBeTruthy();
    expect(screen.getByText('shell')).toBeTruthy();
    expect(screen.getByText('rm -rf /tmp/x')).toBeTruthy();
    // 三个操作按钮齐全（mock t 返回 key 作为按钮文案）
    expect(screen.getByRole('button', { name: 'agent.approveOnce' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'agent.approveSession' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'agent.deny' })).toBeTruthy();
    // 点击「允许一次」→ 捕获当前连接，断言发出 approval_response
    const ws = wsInstance!;
    fireEvent.click(screen.getByRole('button', { name: 'agent.approveOnce' }));
    expect(
      ws.sent.some(
        (s) =>
          s.includes('"type":"approval_response"') &&
          s.includes('"request_id":"req1"') &&
          s.includes('"approved":true') &&
          s.includes('"remember":"none"'),
      ),
    ).toBe(true);
    // 卡片变为已允许：操作按钮消失、状态文案出现
    expect(screen.queryByRole('button', { name: 'agent.approveOnce' })).toBeNull();
    expect(screen.getByText(/agent.approved/)).toBeTruthy();
  });

  it('denies approval and approve-session sends remember=session', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({ type: 'approval_request', request_id: 'req2', tool: 'shell', summary: 'echo hi', args_preview: '{}' });
    });
    // 拒绝：approved=false, remember=none，卡片变为已拒绝
    const ws = wsInstance!;
    fireEvent.click(screen.getByRole('button', { name: 'agent.deny' }));
    expect(
      ws.sent.some((s) => s.includes('"type":"approval_response"') && s.includes('"request_id":"req2"') && s.includes('"approved":false') && s.includes('"remember":"none"')),
    ).toBe(true);
    expect(screen.getByText(/agent.denied/)).toBeTruthy();
    // 新的审批请求 → 点击「本会话允许」：remember=session
    act(() => {
      wsInstance!.emit({ type: 'approval_request', request_id: 'req3', tool: 'write_file', summary: 'write x', args_preview: '{}' });
    });
    const ws2 = wsInstance!;
    fireEvent.click(screen.getByRole('button', { name: 'agent.approveSession' }));
    expect(
      ws2.sent.some((s) => s.includes('"type":"approval_response"') && s.includes('"request_id":"req3"') && s.includes('"approved":true') && s.includes('"remember":"session"')),
    ).toBe(true);
    expect(screen.getByText(/agent.approved/)).toBeTruthy();
  });

  it('expires pending approval cards on done frame and unlocks send', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({
        type: 'approval_request',
        request_id: 'req1',
        tool: 'shell',
        summary: 'rm -rf /tmp/x',
        args_preview: '{}',
      });
    });
    // 输入文本后发送按钮仍被 pending 审批禁用
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: 'hi' } });
    expect((screen.getByRole('button', { name: 'agent.send' }) as HTMLButtonElement).disabled).toBe(true);
    // done 帧到达（服务端 5 分钟审批超时按 deny 继续回合）→ 卡片过期、发送解锁
    act(() => {
      wsInstance!.emit({ type: 'done' });
    });
    expect(screen.queryByRole('button', { name: 'agent.approveOnce' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'agent.deny' })).toBeNull();
    expect(screen.getByText('agent.approvalExpired')).toBeTruthy();
    expect((screen.getByRole('button', { name: 'agent.send' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('expires pending approval cards on stop and unlocks send', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    // 危险工具调用进入 running → 服务端发审批请求挂起回合
    act(() => {
      wsInstance!.emit({ type: 'tool_call', id: 'c1', name: 'shell', args: '{}' });
    });
    act(() => {
      wsInstance!.emit({
        type: 'approval_request',
        request_id: 'req2',
        tool: 'shell',
        summary: 'rm -rf /tmp/x',
        args_preview: '{}',
      });
    });
    expect(screen.getByText('agent.running')).toBeTruthy();
    // 点击前捕获当前连接（stop 触发 state 更新后 WS 实例轮换，cancel 发在旧实例）
    const ws = wsInstance!;
    act(() => {
      screen.getByRole('button', { name: 'agent.stop' }).click();
    });
    expect(ws.sent.some((s) => s.includes('"type":"cancel"'))).toBe(true);
    expect(screen.queryByText('agent.running')).toBeNull();
    // 停止 → 卡片过期：操作按钮消失、过期文案出现
    expect(screen.queryByRole('button', { name: 'agent.approveOnce' })).toBeNull();
    expect(screen.getByText('agent.approvalExpired')).toBeTruthy();
    // 输入文本后发送按钮恢复可用
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: 'hi' } });
    expect((screen.getByRole('button', { name: 'agent.send' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('expires pending approval cards on disconnect (onclose) and unlocks send', async () => {
    (listAgentMessages as Mock).mockResolvedValue([]);
    renderChat();
    act(() => {
      wsInstance!.emit({
        type: 'approval_request',
        request_id: 'req1',
        tool: 'shell',
        summary: 'rm -rf /tmp/x',
        args_preview: '{}',
      });
    });
    // 断线：服务端 turn 被 drop、审批按 deny 落定；重连后历史 refetch 若失败，
    // 本地卡片不置终态会让 hasPendingApproval 恒 true → 发送按钮永久锁死
    act(() => {
      wsInstance!.onclose?.();
    });
    // 断线 → 卡片过期：操作按钮消失、过期文案出现
    expect(screen.queryByRole('button', { name: 'agent.approveOnce' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'agent.deny' })).toBeNull();
    expect(screen.getByText('agent.approvalExpired')).toBeTruthy();
    // 输入文本后发送按钮恢复可用
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: 'hi' } });
    expect((screen.getByRole('button', { name: 'agent.send' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('shows mention popup on @ and sends refs with message', async () => {
    (listWorkspaceFiles as Mock).mockResolvedValue({ files: ['src/main.rs'] });
    renderChat();
    // 输入 @mai → @ 弹层出现，列出匹配文件
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: '@mai' } });
    expect(await screen.findByText('src/main.rs')).toBeTruthy();
    // 选中文件 → @query 段从文本移除，路径进引用 chip
    fireEvent.click(screen.getByText('src/main.rs'));
    expect(screen.getByText('@src/main.rs')).toBeTruthy();
    expect((screen.getByPlaceholderText('agent.inputPlaceholder') as HTMLTextAreaElement).value).toBe('');
    // 输入消息并发送 → WS 帧带 refs
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: '检查这个文件' } });
    const ws = wsInstance!;
    fireEvent.click(screen.getByRole('button', { name: 'agent.send' }));
    expect(
      ws.sent.some((s) => s.includes('"type":"user_message"') && s.includes('"refs":["src/main.rs"]')),
    ).toBe(true);
  });

  it('selects the highlighted mention item on Enter without sending', async () => {
    (listWorkspaceFiles as Mock).mockResolvedValue({ files: ['src/main.rs'] });
    renderChat();
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: '@mai' } });
    expect(await screen.findByText('src/main.rs')).toBeTruthy();
    const textarea = screen.getByPlaceholderText('agent.inputPlaceholder') as HTMLTextAreaElement;
    // 弹层打开时按 Enter → 选中高亮项，而非发送消息
    fireEvent.keyDown(textarea, { key: 'Enter' });
    expect(screen.getByText('@src/main.rs')).toBeTruthy();
    expect(textarea.value).toBe('');
    // 任何连接的 WS 实例都没有发出 user_message 帧（断言覆盖 i18n mock 引发的实例轮换）
    expect(wsInstances.every((w) => !w.sent.some((s) => s.includes('"type":"user_message"')))).toBe(true);
    // 弹层关闭
    expect(screen.queryByText('src/main.rs')).toBeNull();
  });

  it('moves highlight with ArrowDown and selects the second item on Enter', async () => {
    (listWorkspaceFiles as Mock).mockResolvedValue({ files: ['src/a.rs', 'src/b.rs'] });
    renderChat();
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: '@sr' } });
    expect(await screen.findByText('src/a.rs')).toBeTruthy();
    const textarea = screen.getByPlaceholderText('agent.inputPlaceholder') as HTMLTextAreaElement;
    // ↓ 移动高亮到第二项 → Enter 选中 src/b.rs
    fireEvent.keyDown(textarea, { key: 'ArrowDown' });
    fireEvent.keyDown(textarea, { key: 'Enter' });
    expect(await screen.findByText('@src/b.rs')).toBeTruthy();
    expect(screen.queryByText('@src/a.rs')).toBeNull();
    expect(textarea.value).toBe('');
  });

  it('closes the mention popup on Escape', async () => {
    (listWorkspaceFiles as Mock).mockResolvedValue({ files: ['src/main.rs'] });
    renderChat();
    fireEvent.change(screen.getByPlaceholderText('agent.inputPlaceholder'), { target: { value: '@mai' } });
    expect(await screen.findByText('src/main.rs')).toBeTruthy();
    fireEvent.keyDown(screen.getByPlaceholderText('agent.inputPlaceholder'), { key: 'Escape' });
    expect(screen.queryByText('src/main.rs')).toBeNull();
  });

  it('removes a single chip independently', async () => {
    (listWorkspaceFiles as Mock).mockResolvedValue({ files: ['src/a.rs', 'src/b.rs'] });
    renderChat();
    const textarea = screen.getByPlaceholderText('agent.inputPlaceholder') as HTMLTextAreaElement;
    // 依次选择两个文件 → 两个 chip
    fireEvent.change(textarea, { target: { value: '@sr' } });
    fireEvent.click(await screen.findByText('src/a.rs'));
    fireEvent.change(textarea, { target: { value: '@sr' } });
    fireEvent.click(await screen.findByText('src/b.rs'));
    expect(screen.getByText('@src/a.rs')).toBeTruthy();
    expect(screen.getByText('@src/b.rs')).toBeTruthy();
    // 单独删除 src/a.rs 的 chip，src/b.rs 保留
    const chipA = screen.getByText('@src/a.rs');
    const removeBtn = chipA.parentElement!.querySelector('button')!;
    fireEvent.click(removeBtn);
    expect(screen.queryByText('@src/a.rs')).toBeNull();
    expect(screen.getByText('@src/b.rs')).toBeTruthy();
  });
});

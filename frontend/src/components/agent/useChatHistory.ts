import { useCallback, useEffect, useRef, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { listAgentMessages } from '../../api/client';
import type { AgentMessagesPage } from '../../api/client';
import type { AgentMessage } from '../../types';
import type { ChatItem } from './types';
import {
  compactionSkippedIndices,
  historyToChatItems,
  historyToChatItemsWithSkip,
  prependSkip,
} from './history';
import { mergePages } from './subagent';

const EARLIER_PAGE_SIZE = 200;

function historyRows(h: AgentMessagesPage | AgentMessage[] | undefined): AgentMessage[] {
  if (!h) return [];
  return Array.isArray(h) ? h : (h.messages ?? []);
}

function historyHasMore(h: AgentMessagesPage | AgentMessage[] | undefined): boolean {
  return !Array.isArray(h) && (h?.has_more ?? false);
}

export interface UseChatHistoryOptions {
  sessionId: string;
  items: ChatItem[];
  setItems: React.Dispatch<React.SetStateAction<ChatItem[]>>;
  streamingIdxRef: React.MutableRefObject<number | null>;
  scrollRef: React.MutableRefObject<HTMLDivElement | null>;
  earlierButtonRef: React.MutableRefObject<HTMLDivElement | null>;
  lastButtonHeightRef: React.MutableRefObject<number>;
}

export interface UseChatHistoryReturn {
  hasMore: boolean;
  loadingEarlier: boolean;
  loadEarlier: () => Promise<void>;
  loadedRawRef: React.MutableRefObject<AgentMessage[]>;
  earlierCountRef: React.MutableRefObject<number>;
  loadedRef: React.MutableRefObject<boolean>;
  partialLoadRef: React.MutableRefObject<boolean>;
  reconcileRef: React.MutableRefObject<boolean>;
  historyRef: React.MutableRefObject<AgentMessagesPage | AgentMessage[] | undefined>;
}

export function useChatHistory(opts: UseChatHistoryOptions): UseChatHistoryReturn {
  const { sessionId, items, setItems, streamingIdxRef, scrollRef, earlierButtonRef, lastButtonHeightRef } = opts;

  const [hasMore, setHasMore] = useState(false);
  const [loadingEarlier, setLoadingEarlier] = useState(false);
  const loadingEarlierRef = useRef(false);
  const loadedRawRef = useRef<AgentMessage[]>([]);
  const earlierCountRef = useRef(0);
  const loadedRef = useRef(false);
  // 「本次装载拿到的历史可能不完整」——由 useAgentWs 在收到 turn_state{running:true}
  // 时置位（服务端确实在跑，回合中途的消息还没全落库），done 到达时触发一次对账重载。
  const partialLoadRef = useRef(false);
  const reconcileRef = useRef(false);
  const historyRef = useRef<AgentMessagesPage | AgentMessage[] | undefined>(undefined);
  const itemsRef = useRef<ChatItem[]>(items);
  const prevHasMoreRef = useRef<boolean | null>(null);

  useEffect(() => {
    itemsRef.current = items;
  }, [items]);

  useEffect(() => {
    if (prevHasMoreRef.current === null) {
      prevHasMoreRef.current = hasMore;
      return;
    }
    if (prevHasMoreRef.current === hasMore) return;
    prevHasMoreRef.current = hasMore;
    const el = scrollRef.current;
    if (!el) return;
    const h = lastButtonHeightRef.current;
    if (h <= 0) return;
    el.scrollTop += hasMore ? h : -h;
  }, [hasMore, scrollRef, lastButtonHeightRef]);

  const { data: history } = useQuery<AgentMessagesPage | AgentMessage[]>({
    queryKey: ['agent-messages', sessionId],
    queryFn: () => listAgentMessages(sessionId),
    refetchOnMount: 'always',
    refetchOnWindowFocus: false,
  });

  useEffect(() => {
    historyRef.current = history;
    const rows = historyRows(history);
    if (!history) return;
    if (loadedRef.current && !(itemsRef.current.length === 0 && rows.length > 0)) return;
    const isReconcileReload = reconcileRef.current;
    reconcileRef.current = false;
    if (!isReconcileReload && itemsRef.current.length > 0) {
      loadedRef.current = true;
      return;
    }
    loadedRef.current = true;
    if (isReconcileReload) {
      const earlierRows = loadedRawRef.current.slice(0, earlierCountRef.current);
      const mergedRaw = [...earlierRows, ...rows];
      loadedRawRef.current = mergedRaw;
      setItems(historyToChatItemsWithSkip(mergedRaw, compactionSkippedIndices(mergedRaw)));
      return;
    }
    // running 不再按历史末行推断。服务端建连即推 turn_state 真值（见 useAgentWs），
    // 而「末行是 tool_calls/tool_result」是回合正常结束后的常态（收尾文本未落库），
    // 抢在真值到达前猜 running 只会让输入框闪一下再被纠正回来。
    setItems(historyToChatItems(rows));
    loadedRawRef.current = rows;
    earlierCountRef.current = 0;
    setHasMore(historyHasMore(history));
  }, [history, setItems]);

  const loadEarlier = useCallback(async () => {
    if (loadingEarlierRef.current) return;
    const oldestId = loadedRawRef.current[0]?.id;
    if (!oldestId) return;
    loadingEarlierRef.current = true;
    setLoadingEarlier(true);
    lastButtonHeightRef.current = earlierButtonRef.current?.offsetHeight ?? 0;
    try {
      const page = await listAgentMessages(sessionId, {
        before: oldestId,
        limit: EARLIER_PAGE_SIZE,
      });
      if (page.messages.length === 0) {
        setHasMore(false);
        return;
      }
      const olderItems = historyToChatItemsWithSkip(page.messages, prependSkip(page.messages, loadedRawRef.current));
      earlierCountRef.current += page.messages.length;
      loadedRawRef.current = [...page.messages, ...loadedRawRef.current];
      setHasMore(page.has_more);
      setItems((prev) => {
        const { items: merged, absorbedIndexes } = mergePages(olderItems, prev);
        if (streamingIdxRef.current !== null) {
          let shift = olderItems.length;
          for (const i of absorbedIndexes) {
            if (i < streamingIdxRef.current) shift -= 1;
          }
          streamingIdxRef.current += shift;
        }
        return merged;
      });
    } catch {
      // silent
    } finally {
      loadingEarlierRef.current = false;
      setLoadingEarlier(false);
    }
  }, [sessionId, setItems, streamingIdxRef, earlierButtonRef, lastButtonHeightRef]);

  return {
    hasMore,
    loadingEarlier,
    loadEarlier,
    loadedRawRef,
    earlierCountRef,
    loadedRef,
    partialLoadRef,
    reconcileRef,
    historyRef,
  };
}

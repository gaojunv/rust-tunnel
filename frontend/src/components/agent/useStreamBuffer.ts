import { useCallback, useEffect, useRef } from 'react';
import type { ChatItem } from './types';
import { appendChildStream, parseChunkKey } from './subagent';
import { nextLiveItemId } from './liveId';

/** 流式 chunk 合并 flush 间隔：token 级 WS 帧攒批后一次性写 state，避免每 token 全列表重渲染。 */
export const STREAM_FLUSH_MS = 50;

export interface UseStreamBufferReturn {
  chunkBufRef: React.MutableRefObject<Map<string, string>>;
  chunkFlushTimerRef: React.MutableRefObject<ReturnType<typeof setTimeout> | null>;
  streamingIdxRef: React.MutableRefObject<number | null>;
  streamingKindRef: React.MutableRefObject<'assistant' | 'thought' | null>;
  subStreamRef: React.MutableRefObject<Map<string, { idx: number; kind: 'assistant' | 'thought' }>>;
  flushChunks: () => void;
  breakStream: () => void;
  breakSubStream: (parentToolId: string) => void;
  scheduleChunkFlush: () => void;
}

export function useStreamBuffer(opts: {
  setItems: React.Dispatch<React.SetStateAction<ChatItem[]>>;
}): UseStreamBufferReturn {
  const { setItems } = opts;
  const chunkBufRef = useRef<Map<string, string>>(new Map());
  const chunkFlushTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const streamingIdxRef = useRef<number | null>(null);
  const streamingKindRef = useRef<'assistant' | 'thought' | null>(null);
  const subStreamRef = useRef<Map<string, { idx: number; kind: 'assistant' | 'thought' }>>(new Map());

  const flushChunks = useCallback(() => {
    if (chunkFlushTimerRef.current) {
      clearTimeout(chunkFlushTimerRef.current);
      chunkFlushTimerRef.current = null;
    }
    if (chunkBufRef.current.size === 0) return;
    const buf = chunkBufRef.current;
    chunkBufRef.current = new Map();
    setItems((prev) => {
      let next = prev;
      for (const [key, content] of buf) {
        const { parent, kind } = parseChunkKey(key);
        if (!parent) {
          const idx = streamingIdxRef.current;
          if (idx !== null && next[idx]?.kind === kind) {
            next = next.map((it, i) => (i === idx ? { ...it, content: it.content + content } : it));
          } else {
            streamingIdxRef.current = next.length;
            next = [...next, { id: nextLiveItemId(), kind, content }];
          }
          continue;
        }
        const res = appendChildStream(next, parent, kind, content, subStreamRef.current.get(parent) ?? null);
        if (res.attached) {
          next = res.state;
          if (res.stream) subStreamRef.current.set(parent, res.stream);
        } else {
          next = [...next, { id: nextLiveItemId(), kind, content, parentToolId: parent }];
        }
      }
      return next;
    });
  }, [setItems]);

  const breakSubStream = useCallback(
    (parentToolId: string) => {
      setItems((prev) => {
        subStreamRef.current.delete(parentToolId);
        return prev;
      });
    },
    [setItems],
  );

  const breakStream = useCallback(() => {
    setItems((prev) => {
      streamingIdxRef.current = null;
      streamingKindRef.current = null;
      return prev;
    });
  }, [setItems]);

  const scheduleChunkFlush = useCallback(() => {
    if (chunkFlushTimerRef.current) return;
    chunkFlushTimerRef.current = globalThis.setTimeout(() => {
      chunkFlushTimerRef.current = null;
      flushChunks();
    }, STREAM_FLUSH_MS);
  }, [flushChunks]);

  useEffect(
    () => () => {
      if (chunkFlushTimerRef.current) {
        clearTimeout(chunkFlushTimerRef.current);
        chunkFlushTimerRef.current = null;
      }
      chunkBufRef.current = new Map();
    },
    [],
  );

  return {
    chunkBufRef,
    chunkFlushTimerRef,
    streamingIdxRef,
    streamingKindRef,
    subStreamRef,
    flushChunks,
    breakStream,
    breakSubStream,
    scheduleChunkFlush,
  };
}

import { useCallback, useMemo, useState } from "react";

export type NoteHistory = {
  current: string | null;
  canBack: boolean;
  canForward: boolean;
  navigate: (key: string) => void;
  back: () => void;
  forward: () => void;
  remove: (key: string) => void;
  replace: (oldKey: string, newKey: string) => void;
};

export function useNoteHistory(): NoteHistory {
  const [current, setCurrent] = useState<string | null>(null);
  const [past, setPast] = useState<string[]>([]);
  const [future, setFuture] = useState<string[]>([]);

  const canBack = past.length > 0;
  const canForward = future.length > 0;

  const navigate = useCallback(
    (key: string) => {
      if (key === current) return;
      if (current !== null) {
        setPast((p) => [...p, current]);
      }
      setFuture([]);
      setCurrent(key);
    },
    [current],
  );

  const back = useCallback(() => {
    if (past.length === 0 || current === null) return;
    const prevKey = past[past.length - 1];
    setPast((p) => p.slice(0, -1));
    setFuture((f) => [...f, current]);
    setCurrent(prevKey);
  }, [current, past]);

  const forward = useCallback(() => {
    if (future.length === 0 || current === null) return;
    const nextKey = future[future.length - 1];
    setFuture((f) => f.slice(0, -1));
    setPast((p) => [...p, current]);
    setCurrent(nextKey);
  }, [current, future]);

  const remove = useCallback((key: string) => {
    setPast((p) => p.filter((k) => k !== key));
    setFuture((f) => f.filter((k) => k !== key));
    setCurrent((c) => (c === key ? null : c));
  }, []);

  const replace = useCallback((oldKey: string, newKey: string) => {
    if (oldKey === newKey) return;
    setCurrent((c) => (c === oldKey ? newKey : c));
    setPast((p) => p.map((k) => (k === oldKey ? newKey : k)));
    setFuture((f) => f.map((k) => (k === oldKey ? newKey : k)));
  }, []);

  return useMemo(
    () => ({
      current,
      canBack,
      canForward,
      navigate,
      back,
      forward,
      remove,
      replace,
    }),
    [current, canBack, canForward, navigate, back, forward, remove, replace],
  );
}

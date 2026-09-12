// 轻量全局 toast 存储：订阅者模式（listener set），可在非 React 模块
// （如 use-sync.ts 的工具函数）直接调用，无需引入新依赖。
export type ToastKind = "success" | "error" | "info";

export type ToastItem = {
  id: number;
  kind: ToastKind;
  message: string;
};

type Listener = (items: ToastItem[]) => void;

const listeners = new Set<Listener>();
let items: ToastItem[] = [];
let nextId = 1;
const timers = new Map<number, ReturnType<typeof setTimeout>>();

/** 同时展示的上限：超出时丢弃最早的，保留最新 N 条 */
export const TOAST_MAX_ITEMS = 5;

const TOAST_TTL_MS: Record<ToastKind, number> = {
  success: 3000,
  info: 3000,
  error: 5000,
};

function emit(): void {
  const snapshot = [...items];
  for (const l of listeners) l(snapshot);
}

export function subscribeToast(listener: Listener): () => void {
  listeners.add(listener);
  listener([...items]);
  return () => {
    listeners.delete(listener);
  };
}

export function getToasts(): ToastItem[] {
  return [...items];
}

export function dismissToast(id: number): void {
  const t = timers.get(id);
  if (t !== undefined) {
    clearTimeout(t);
    timers.delete(id);
  }
  if (items.some((i) => i.id === id)) {
    items = items.filter((i) => i.id !== id);
    emit();
  }
}

function push(kind: ToastKind, message: string): number {
  const id = nextId++;
  items = [...items, { id, kind, message }].slice(-TOAST_MAX_ITEMS);
  emit();
  timers.set(
    id,
    setTimeout(() => dismissToast(id), TOAST_TTL_MS[kind]),
  );
  return id;
}

export const toast = {
  success(message: string): number {
    return push("success", message);
  },
  error(message: string): number {
    return push("error", message);
  },
  info(message: string): number {
    return push("info", message);
  },
};

/** 仅供单测：清空全部 toast（含待触发的自动消失计时器）。 */
export function resetToastStore(): void {
  for (const t of timers.values()) clearTimeout(t);
  timers.clear();
  items = [];
  emit();
}

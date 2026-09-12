// @vitest-environment jsdom
// ToastHost 组件单测：react-dom 直测，不引入 testing-library
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { ToastHost } from "./ToastHost";
import { resetToastStore, toast } from "@/lib/toast";

// 告诉 React 当前是 act 测试环境，消除 act(...) 警告
(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root | null = null;
let container: HTMLDivElement | null = null;

function render() {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  act(() => {
    root?.render(<ToastHost />);
  });
}

function items(): HTMLElement[] {
  return [...document.body.querySelectorAll<HTMLElement>('[data-testid="toast-item"]')];
}

beforeEach(() => {
  vi.useFakeTimers();
  resetToastStore();
});

afterEach(() => {
  act(() => {
    root?.unmount();
  });
  container?.remove();
  root = null;
  container = null;
  document.body.innerHTML = "";
  resetToastStore();
  vi.useRealTimers();
});

describe("ToastHost", () => {
  it("无 toast 时不渲染容器", () => {
    render();
    expect(document.body.querySelector('[data-testid="toast-host"]')).toBeNull();
  });

  it("推送后渲染堆叠的多条 toast", () => {
    render();
    act(() => {
      toast.success("保存成功");
      toast.error("保存失败");
    });
    const list = items();
    expect(list).toHaveLength(2);
    expect(list[0]?.dataset.kind).toBe("success");
    expect(list[0]?.textContent).toContain("保存成功");
    expect(list[1]?.dataset.kind).toBe("error");
  });

  it("点击 × 手动关闭单条", () => {
    render();
    act(() => {
      toast.success("a");
      toast.info("b");
    });
    expect(items()).toHaveLength(2);
    const closeBtn = items()[0]?.querySelector('button[aria-label="关闭通知"]') as HTMLElement;
    act(() => {
      closeBtn.click();
    });
    expect(items().map((el) => el.dataset.kind)).toEqual(["info"]);
  });

  it("自动消失计时到期后移除", () => {
    render();
    act(() => {
      toast.success("稍后消失");
    });
    expect(items()).toHaveLength(1);
    act(() => {
      vi.advanceTimersByTime(3000);
    });
    expect(items()).toHaveLength(0);
  });
});

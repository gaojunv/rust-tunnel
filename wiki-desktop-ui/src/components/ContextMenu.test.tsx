// @vitest-environment jsdom
// ContextMenu 组件单测：react-dom 直测，不引入 testing-library
import { afterEach, describe, expect, it, vi } from "vitest";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { ContextMenu, ContextMenuItem } from "./ContextMenu";

// 告诉 React 当前是 act 测试环境，消除 act(...) 警告
(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root | null = null;
let container: HTMLDivElement | null = null;

function render(ui: React.ReactNode) {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  act(() => {
    root?.render(ui);
  });
}

afterEach(() => {
  act(() => {
    root?.unmount();
  });
  container?.remove();
  root = null;
  container = null;
  document.body.innerHTML = "";
  vi.restoreAllMocks();
});

function menuEl(): HTMLElement | null {
  return document.body.querySelector('[data-testid="context-menu"]');
}

describe("ContextMenu", () => {
  it("渲染菜单项并定位在鼠标处", () => {
    const onClose = vi.fn();
    const onSelect = vi.fn();
    render(
      <ContextMenu x={120} y={80} onClose={onClose}>
        <ContextMenuItem label="打开" onSelect={onSelect} />
        <ContextMenuItem label="删除" danger onSelect={onSelect} />
      </ContextMenu>,
    );
    const menu = menuEl();
    expect(menu).not.toBeNull();
    expect(menu?.style.left).toBe("120px");
    expect(menu?.style.top).toBe("80px");
    const items = menu?.querySelectorAll('[role="menuitem"]') ?? [];
    expect(items.length).toBe(2);
    expect(items[0]?.textContent).toBe("打开");
    expect(items[1]?.textContent).toBe("删除");
  });

  it("点击菜单项触发回调", () => {
    const onClose = vi.fn();
    const onSelect = vi.fn();
    render(
      <ContextMenu x={10} y={10} onClose={onClose}>
        <ContextMenuItem label="重命名" onSelect={onSelect} />
      </ContextMenu>,
    );
    const item = menuEl()?.querySelector('[role="menuitem"]') as HTMLElement;
    act(() => {
      item.click();
    });
    expect(onSelect).toHaveBeenCalledTimes(1);
  });

  it("Esc 关闭菜单", () => {
    const onClose = vi.fn();
    render(
      <ContextMenu x={10} y={10} onClose={onClose}>
        <ContextMenuItem label="打开" onSelect={() => {}} />
      </ContextMenu>,
    );
    act(() => {
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("点击外部关闭菜单，点击内部不关闭", () => {
    const onClose = vi.fn();
    render(
      <ContextMenu x={10} y={10} onClose={onClose}>
        <ContextMenuItem label="打开" onSelect={() => {}} />
      </ContextMenu>,
    );
    const menu = menuEl();
    expect(menu).not.toBeNull();
    // 点击菜单内部：不关闭
    act(() => {
      menu?.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
    });
    expect(onClose).not.toHaveBeenCalled();
    // 点击外部：关闭
    act(() => {
      document.body.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true }));
    });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("危险项带红色样式", () => {
    render(
      <ContextMenu x={10} y={10} onClose={() => {}}>
        <ContextMenuItem label="删除" danger onSelect={() => {}} />
      </ContextMenu>,
    );
    const item = menuEl()?.querySelector('[role="menuitem"]');
    expect(item?.className).toContain("text-destructive");
  });
});

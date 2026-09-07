/**
 * @vitest-environment node
 */
import { describe, it, expect } from "vitest";
import {
  AGENT_MODES,
  DEFAULT_AGENT_MODE,
  MODE_PRESETS,
  isAgentMode,
  modeToTurnOverrides,
} from "./mode-presets";

describe("档位元信息", () => {
  it("三档齐全且顺序为 只读/自动/全权", () => {
    expect(AGENT_MODES).toEqual(["readonly", "auto", "full"]);
    for (const m of AGENT_MODES) {
      expect(MODE_PRESETS[m]).toBeDefined();
    }
  });

  it("中文 label 与一句话说明齐全", () => {
    expect(MODE_PRESETS.readonly?.label).toBe("只读");
    expect(MODE_PRESETS.auto?.label).toBe("自动");
    expect(MODE_PRESETS.full?.label).toBe("全权");
    for (const m of AGENT_MODES) {
      const p = MODE_PRESETS[m] as { label: string; description: string };
      expect(p.label.length).toBeGreaterThan(0);
      expect(p.description.length).toBeGreaterThan(0);
    }
  });

  it("危险等级：只有 full 为 high", () => {
    expect(MODE_PRESETS.readonly?.danger).toBe("low");
    expect(MODE_PRESETS.auto?.danger).toBe("medium");
    expect(MODE_PRESETS.full?.danger).toBe("high");
  });

  it("默认档为 auto", () => {
    expect(DEFAULT_AGENT_MODE).toBe("auto");
  });
});

describe("isAgentMode", () => {
  it("三档合法，其余非法", () => {
    expect(isAgentMode("readonly")).toBe(true);
    expect(isAgentMode("auto")).toBe(true);
    expect(isAgentMode("full")).toBe(true);
    expect(isAgentMode("")).toBe(false);
    expect(isAgentMode("readOnly")).toBe(false);
    expect(isAgentMode("AUTO")).toBe(false);
    // 旧脏数据归一示例：非法值由调用方回退默认
    const fromStore = "legacy-plan";
    expect(isAgentMode(fromStore) ? fromStore : DEFAULT_AGENT_MODE).toBe("auto");
  });
});

describe("modeToTurnOverrides", () => {
  it("readonly → on-request + readOnly", () => {
    const o = modeToTurnOverrides("readonly", "/vault");
    expect(o.approvalPolicy).toBe("on-request");
    expect(o.sandboxPolicy).toEqual({ type: "readOnly", networkAccess: false });
  });

  it("auto → on-request + workspaceWrite（含 vaultRoot、可写根断网、tmp 排除）", () => {
    const o = modeToTurnOverrides("auto", "/vault/知识库");
    expect(o.approvalPolicy).toBe("on-request");
    expect(o.sandboxPolicy).toEqual({
      type: "workspaceWrite",
      writableRoots: ["/vault/知识库"],
      networkAccess: false,
      excludeTmpdirEnvVar: true,
      excludeSlashTmp: true,
    });
  });

  it("full → never + dangerFullAccess（警示档）", () => {
    const o = modeToTurnOverrides("full", "/vault");
    expect(o.approvalPolicy).toBe("never");
    expect(o.sandboxPolicy).toEqual({ type: "dangerFullAccess" });
  });

  it("每次返回新对象（调用方修改不污染下次）", () => {
    const a = modeToTurnOverrides("auto", "/vault");
    const b = modeToTurnOverrides("auto", "/vault");
    expect(a).not.toBe(b);
    expect(a.sandboxPolicy).not.toBe(b.sandboxPolicy);
    if (a.sandboxPolicy.type === "workspaceWrite") {
      a.sandboxPolicy.writableRoots.push("/evil");
    }
    const c = modeToTurnOverrides("auto", "/vault");
    expect(c.sandboxPolicy).toEqual({
      type: "workspaceWrite",
      writableRoots: ["/vault"],
      networkAccess: false,
      excludeTmpdirEnvVar: true,
      excludeSlashTmp: true,
    });
  });

  it("JSON 可序列化（可直接透传给 bridge）", () => {
    for (const m of AGENT_MODES) {
      const o = modeToTurnOverrides(m, "/vault");
      expect(() => JSON.stringify(o)).not.toThrow();
      expect(JSON.parse(JSON.stringify(o))).toEqual(o);
    }
  });
});

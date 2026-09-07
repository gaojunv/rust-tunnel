/**
 * 审批模式三档预设 —— 纯函数（vitest node 环境可测）
 *
 * 注意：本文件不得 import vendor types（只有 client.ts 可以）。
 * 以下结构化类型的字段名/值字面量对照 vendor 实测结论手写保持一致：
 * - vendor `AskForApproval` = "untrusted" | "on-request" | { granular: ... } | "never"
 *   （见 src/lib/agent/types/v2/AskForApproval.ts）
 * - vendor `SandboxPolicy` =
 *     { type: "dangerFullAccess" }
 *   | { type: "readOnly", networkAccess: boolean }
 *   | { type: "externalSandbox", networkAccess: ... }
 *   | { type: "workspaceWrite", writableRoots: string[], networkAccess: boolean,
 *       excludeTmpdirEnvVar: boolean, excludeSlashTmp: boolean }
 *   （见 src/lib/agent/types/v2/SandboxPolicy.ts；`AbsolutePathBuf` 即 string）
 *
 * 2E 接线时把 `modeToTurnOverrides()` 的返回值直接赋给 `TurnStartParams` 的
 * `approvalPolicy` / `sandboxPolicy` 字段即可（结构兼容）。
 */

/** 三档审批模式 key（与 store.ts `AgentThreadRecord.mode` 存的值对齐） */
export type AgentMode = "readonly" | "auto" | "full";

/** 三档顺序（ModeBar 分段控件渲染用） */
export const AGENT_MODES: AgentMode[] = ["readonly", "auto", "full"];

/** 默认档：自动（知识库内可写 + 危险操作审批，兼顾效率与安全） */
export const DEFAULT_AGENT_MODE: AgentMode = "auto";

/** 校验外部字符串是否为合法档位（store 回读时做归一用） */
export function isAgentMode(value: string): value is AgentMode {
  return value === "readonly" || value === "auto" || value === "full";
}

/**
 * per-turn 审批策略 —— 对照 vendor `AskForApproval` 的字符串分支子集。
 * granular 对象分支暂不用（如需细粒度再扩展）。
 */
export type TurnApprovalPolicy = "on-request" | "never" | "untrusted";

/**
 * per-turn 沙箱策略 —— 对照 vendor `SandboxPolicy` 联合类型的三个常用分支
 *（`externalSandbox` 分支面板暂不用，未收录）。
 */
export type TurnSandboxPolicy =
  | { type: "readOnly"; networkAccess: boolean }
  | {
      type: "workspaceWrite";
      writableRoots: string[];
      networkAccess: boolean;
      excludeTmpdirEnvVar: boolean;
      excludeSlashTmp: boolean;
    }
  | { type: "dangerFullAccess" };

/** 可直接赋给 `TurnStartParams` 对应字段的 per-turn 覆盖 */
export type TurnOverrides = {
  approvalPolicy: TurnApprovalPolicy;
  sandboxPolicy: TurnSandboxPolicy;
};

/** 危险等级：full 为 high，调用方（ModeBar）须标红警示 */
export type ModeDanger = "low" | "medium" | "high";

export type ModePreset = {
  mode: AgentMode;
  /** 中文短 label（分段控件展示） */
  label: string;
  /** 一句话说明 */
  description: string;
  danger: ModeDanger;
};

/** 三档预设元信息（中文 label + 说明 + 危险等级） */
export const MODE_PRESETS: Record<AgentMode, ModePreset> = {
  readonly: {
    mode: "readonly",
    label: "只读",
    description: "只能读文件，任何写入与命令执行都会先弹窗审批。",
    danger: "low",
  },
  auto: {
    mode: "auto",
    label: "自动",
    description: "可在当前知识库内读写文件，越界与敏感操作仍需审批。",
    danger: "medium",
  },
  full: {
    mode: "full",
    label: "全权",
    description: "跳过全部审批并完全访问系统，仅在完全信任时使用，风险自负。",
    danger: "high",
  },
};

/**
 * 档位 → per-turn 覆盖参数。
 * 每次调用返回全新对象（调用方随意修改不影响下次调用）。
 */
export function modeToTurnOverrides(mode: AgentMode, vaultRoot: string): TurnOverrides {
  switch (mode) {
    case "readonly":
      return {
        approvalPolicy: "on-request",
        sandboxPolicy: { type: "readOnly", networkAccess: false },
      };
    case "auto":
      return {
        approvalPolicy: "on-request",
        sandboxPolicy: {
          type: "workspaceWrite",
          writableRoots: [vaultRoot],
          networkAccess: false,
          excludeTmpdirEnvVar: true,
          excludeSlashTmp: true,
        },
      };
    case "full":
      return {
        approvalPolicy: "never",
        sandboxPolicy: { type: "dangerFullAccess" },
      };
  }
}

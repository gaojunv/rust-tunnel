/**
 * 斜杠命令注册表 + 纯解析/过滤函数。
 * 纯函数模块：不依赖 DOM / Tauri / React，vitest node 环境可直接测试。
 *
 * 本模块只描述命令与解析，不执行任何动作。执行由 AgentPanel（2E 接线）按 runKind 分发：
 *   - "rpc"    远端调用（/compact→threadCompactStart、/review→reviewStart、
 *              /rename→threadSetName、/archive→threadArchive）
 *   - "ui"     本地界面动作（/model 打开模型菜单、/new 新建会话）
 *   - "prefix" 输入框文本前缀插入 + 提示（/plan 降级近似：协议不支持单轮 plan 模式）
 */

export type CommandRunKind = "rpc" | "ui" | "prefix";

export type Command = {
  /** 命令名（不含前导 /），如 "compact" */
  name: string;
  /** 菜单内展示的中文说明 */
  description: string;
  /** 用法提示（如 /rename <name> 的 "<name>"）；缺参报错时上层可复用该文案 */
  argsHint?: string;
  /** 需要处于活跃会话才可用 */
  needsThread: boolean;
  /** 运行形态，供上层分发 */
  runKind: CommandRunKind;
};

/** 命令注册表（顺序即菜单展示顺序） */
export const COMMANDS: readonly Command[] = [
  {
    name: "compact",
    description: "压缩上下文，清理冗长历史后继续",
    needsThread: true,
    runKind: "rpc",
  },
  {
    name: "review",
    description: "审查改动；无参数=未提交改动，带参数=自定义审查说明",
    argsHint: "[instructions]",
    needsThread: true,
    runKind: "rpc",
  },
  {
    name: "model",
    description: "切换当前会话的模型与推理强度",
    needsThread: false,
    runKind: "ui",
  },
  {
    name: "rename",
    description: "重命名当前会话",
    argsHint: "<name>",
    needsThread: true,
    runKind: "rpc",
  },
  {
    name: "archive",
    description: "归档当前会话",
    needsThread: true,
    runKind: "rpc",
  },
  {
    name: "new",
    description: "新建一个会话",
    needsThread: false,
    runKind: "ui",
  },
  {
    name: "plan",
    description: "请求先输出实施计划（降级为注入前缀）",
    needsThread: false,
    runKind: "prefix",
  },
];

export type ParsedSlash = { cmd: string; arg: string };

/**
 * 解析首个斜杠命令。
 * 仅当文本以 "/" 开头（首字符）且首个 token 非空才返回；否则返回 null。
 * arg 为该行命令名之后的内容（trim 后；可能含空格，如 "/rename my note"）。
 * 边界："/"、"/ "、"/   " → null（无命令 token）。
 */
export function parseSlashInput(text: string): ParsedSlash | null {
  const m = /^\/(\S*)\s*([\s\S]*)$/.exec(text);
  if (!m) return null;
  const cmd = m[1] ?? "";
  if (!cmd) return null;
  return { cmd, arg: (m[2] ?? "").trim() };
}

export type SlashContext = { hasThread: boolean };

/**
 * 过滤可用命令：先按 needsThread 门控（无活跃会话时剔除需线程的命令），
 * 再按前缀/模糊匹配。query 可为 "……当前整行" 或裸 token：
 *   - 以 "/" 开头时取首个空白界 token（去掉 /）做命令名匹配，
 *     因此输入 "/rename foo" 期间菜单仍保持 "/rename" 命中；
 *   - 否则按整串在 命令名/说明/用法 上做子串匹配。
 */
export function filterCommands(query: string, ctx: SlashContext): Command[] {
  const usable = COMMANDS.filter((c) => !c.needsThread || ctx.hasThread);
  const q = query.trim();
  if (!q || q === "/") return [...usable];

  const term = q.startsWith("/") ? (q.slice(1).split(/\s/)[0] ?? "") : q;
  const lower = term.toLowerCase();
  if (!lower) return [...usable];

  return usable.filter((c) => {
    if (c.name.toLowerCase().startsWith(lower)) return true;
    const hay = `${c.description} ${c.argsHint ?? ""}`.toLowerCase();
    return hay.includes(lower);
  });
}

/** 按命令名（不含 /）查注册表；未知命令返回 undefined */
export function findCommand(name: string): Command | undefined {
  return COMMANDS.find((c) => c.name === name);
}

// —— /plan 降级行为 ——
// Codex 协议不支持按单轮切换 plan/collaboration 模式，只能降级近似：
// 往输入框注入「先输出实施计划」前缀文本，并提示切换到「只读/自动」会话预设。

/** /plan 注入的输入框前缀文本 */
export const PLAN_PROMPT_PREFIX = "请先输出实施计划，不要改动文件。";

/** 切换会话预设的提示文案 */
export const PLAN_MODE_HINT =
  "Codex 协议不支持按单轮 plan 模式，已注入「先计划」前缀；建议发送前切换到「只读/自动」会话预设。";

/**
 * 应用 /plan 命令：把输入框里已输入的 "/plan [说明]" 替换为前缀，
 * 若带说明则置于前缀下一行保留，并返回需提示的文案。
 */
export function applyPlanCommand(currentText: string): { value: string; hint: string } {
  const parsed = parseSlashInput(currentText);
  if (parsed?.cmd === "plan" && parsed.arg) {
    return {
      value: `${PLAN_PROMPT_PREFIX}\n${parsed.arg}`,
      hint: PLAN_MODE_HINT,
    };
  }
  const rest = parsed?.cmd === "plan" ? "" : currentText.trim();
  return {
    value: rest ? `${PLAN_PROMPT_PREFIX}\n${rest}` : PLAN_PROMPT_PREFIX,
    hint: PLAN_MODE_HINT,
  };
}

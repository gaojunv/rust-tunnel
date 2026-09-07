/**
 * slash-commands 纯函数测试（node 环境，无需 jsdom）
 */
import { describe, it, expect } from "vitest";
import {
  COMMANDS,
  parseSlashInput,
  filterCommands,
  findCommand,
  applyPlanCommand,
  PLAN_PROMPT_PREFIX,
  PLAN_MODE_HINT,
} from "./slash-commands";

describe("parseSlashInput 解析边界", () => {
  it("纯 / 与带空格的 / 返回 null（无命令 token）", () => {
    expect(parseSlashInput("/")).toBeNull();
    expect(parseSlashInput("/ ")).toBeNull();
    expect(parseSlashInput("/   ")).toBeNull();
  });

  it("空串 / 非斜杠文本返回 null", () => {
    expect(parseSlashInput("")).toBeNull();
    expect(parseSlashInput("hello world")).toBeNull();
  });

  it("非首字符 / 不触发解析", () => {
    expect(parseSlashInput("abc /compact")).toBeNull();
    expect(parseSlashInput(" /compact")).toBeNull();
    expect(parseSlashInput("x/y")).toBeNull();
  });

  it("/x y 拆出 cmd 与 arg", () => {
    expect(parseSlashInput("/compact")).toEqual({ cmd: "compact", arg: "" });
    expect(parseSlashInput("/x y")).toEqual({ cmd: "x", arg: "y" });
    expect(parseSlashInput("/x y z")).toEqual({ cmd: "x", arg: "y z" });
  });

  it("arg 提取保留内部空格、去除首尾空白", () => {
    expect(parseSlashInput("/rename  我的 会话 ")).toEqual({ cmd: "rename", arg: "我的 会话" });
    expect(parseSlashInput("/review fix parser.ts\n")).toEqual({
      cmd: "review",
      arg: "fix parser.ts",
    });
  });
});

describe("filterCommands 过滤与门控", () => {
  it("空查询返回全部命令", () => {
    const all = filterCommands("", { hasThread: true });
    expect(all.map((c) => c.name)).toEqual(COMMANDS.map((c) => c.name));
  });

  it("仅 / 时返回全部（有线程）", () => {
    expect(filterCommands("/", { hasThread: true }).length).toBe(COMMANDS.length);
  });

  it("needsThread 门控：无线程时仅剩 ui/prefix 命令", () => {
    const names = filterCommands("/", { hasThread: false }).map((c) => c.name);
    expect(names).toEqual(["model", "new", "plan"]);
    // 命中已门控命令也返回空
    expect(filterCommands("/compact", { hasThread: false })).toEqual([]);
    expect(filterCommands("/review", { hasThread: false })).toEqual([]);
    expect(filterCommands("compact", { hasThread: false })).toEqual([]);
  });

  it("有线程时前缀命中", () => {
    expect(filterCommands("/comp", { hasThread: true }).map((c) => c.name)).toEqual(["compact"]);
    // "/c"：compact 前缀命中；review 经 argsHint "[instructions]" 子串回退命中
    expect(filterCommands("/c", { hasThread: true }).map((c) => c.name)).toEqual([
      "compact",
      "review",
    ]);
    // 大小写不敏感
    expect(filterCommands("/MODEL", { hasThread: false }).map((c) => c.name)).toEqual(["model"]);
  });

  it("前缀命中同前缀的多命令，保持注册表顺序", () => {
    expect(filterCommands("/re", { hasThread: true }).map((c) => c.name)).toEqual([
      "review",
      "rename",
    ]);
  });

  it("输入带参数的完整命令时仍命中命令本身", () => {
    expect(filterCommands("/rename new title", { hasThread: true }).map((c) => c.name)).toEqual([
      "rename",
    ]);
    expect(filterCommands("/compact now", { hasThread: true }).map((c) => c.name)).toEqual([
      "compact",
    ]);
  });

  it("中文说明子串命中", () => {
    const names = filterCommands("/会话", { hasThread: true }).map((c) => c.name);
    expect(names).toContain("rename");
    expect(names).toContain("archive");
  });

  it("未知命令无匹配", () => {
    expect(filterCommands("/xyz", { hasThread: true })).toEqual([]);
  });
});

describe("findCommand", () => {
  it("按名查询", () => {
    expect(findCommand("compact")?.runKind).toBe("rpc");
    expect(findCommand("new")?.runKind).toBe("ui");
    expect(findCommand("plan")?.runKind).toBe("prefix");
    expect(findCommand("unknown")).toBeUndefined();
  });
});

describe("applyPlanCommand /plan 降级行为", () => {
  it("空输入：插入前缀本身", () => {
    expect(applyPlanCommand("")).toEqual({ value: PLAN_PROMPT_PREFIX, hint: PLAN_MODE_HINT });
  });

  it("仅 /plan：整串替换为前缀", () => {
    expect(applyPlanCommand("/plan")).toEqual({
      value: PLAN_PROMPT_PREFIX,
      hint: PLAN_MODE_HINT,
    });
  });

  it("/plan 带说明：说明保留在前缀下一行", () => {
    expect(applyPlanCommand("/plan 先看 parser 模块")).toEqual({
      value: `${PLAN_PROMPT_PREFIX}\n先看 parser 模块`,
      hint: PLAN_MODE_HINT,
    });
  });

  it("非 /plan 文本：前缀另起一行拼到内容上方", () => {
    expect(applyPlanCommand("请帮忙修复 x")).toEqual({
      value: `${PLAN_PROMPT_PREFIX}\n请帮忙修复 x`,
      hint: PLAN_MODE_HINT,
    });
  });

  it("前缀文本符合设计约定（含句号、含「只读/自动」提示）", () => {
    expect(PLAN_PROMPT_PREFIX).toBe("请先输出实施计划，不要改动文件。");
    expect(PLAN_MODE_HINT).toContain("只读");
  });
});

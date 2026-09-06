import { describe, it, expect } from "vitest";
import {
  createAllowlist,
  firstTokenOfCommand,
  buildAllowlistKey,
  allowlistHas,
  allowlistAdd,
  allowlistClear,
  allowlistKeys,
  allowlistRemove,
} from "./allowlist";

describe("firstTokenOfCommand", () => {
  it("提取首词并去路径", () => {
    expect(firstTokenOfCommand("git status")).toBe("git");
    expect(firstTokenOfCommand("/usr/bin/git status")).toBe("git");
    expect(firstTokenOfCommand("  npm run build ")).toBe("npm");
    expect(firstTokenOfCommand("")).toBe("*");
    expect(firstTokenOfCommand(null)).toBe("*");
  });
});

describe("buildAllowlistKey", () => {
  it("commandExecution 按首词", () => {
    expect(buildAllowlistKey("item/commandExecution/requestApproval", { command: "cargo test --lib" })).toBe(
      "item/commandExecution/requestApproval:cargo",
    );
    expect(buildAllowlistKey("item/commandExecution/requestApproval", { command: null })).toBe(
      "item/commandExecution/requestApproval:*",
    );
  });

  it("fileChange 按 grantRoot", () => {
    expect(buildAllowlistKey("item/fileChange/requestApproval", { grantRoot: "/vault" })).toBe(
      "item/fileChange/requestApproval:/vault",
    );
    expect(buildAllowlistKey("item/fileChange/requestApproval", {})).toBe("item/fileChange/requestApproval:*");
  });

  it("permissions 与 legacy 兜底", () => {
    expect(buildAllowlistKey("item/permissions/requestApproval", {})).toBe("item/permissions/requestApproval:*");
    expect(buildAllowlistKey("execCommandApproval", { command: ["git", "status"] })).toBe("execCommandApproval:git");
    expect(buildAllowlistKey("applyPatchApproval", {})).toBe("applyPatchApproval:*");
    expect(buildAllowlistKey("unknown/method", {})).toBe("unknown/method:*");
  });
});

describe("allowlist 命中与管理", () => {
  it("add/has/remove/clear", () => {
    const al = createAllowlist();
    allowlistAdd(al, "item/commandExecution/requestApproval", { command: "cargo test" });
    expect(allowlistHas(al, "item/commandExecution/requestApproval", { command: "cargo test --lib" })).toBe(true);
    // 不同首词不命中
    expect(allowlistHas(al, "item/commandExecution/requestApproval", { command: "npm run build" })).toBe(false);
    const keys = allowlistKeys(al);
    expect(keys).toEqual(["item/commandExecution/requestApproval:cargo"]);
    allowlistRemove(al, keys[0] as string);
    expect(allowlistHas(al, "item/commandExecution/requestApproval", { command: "cargo test" })).toBe(false);
    allowlistAdd(al, "item/fileChange/requestApproval", { grantRoot: "/a" });
    allowlistClear(al);
    expect(allowlistKeys(al).length).toBe(0);
  });

  it("同一 method 不同 grantRoot 区分", () => {
    const al = createAllowlist();
    allowlistAdd(al, "item/fileChange/requestApproval", { grantRoot: "/vault/a" });
    expect(allowlistHas(al, "item/fileChange/requestApproval", { grantRoot: "/vault/a" })).toBe(true);
    expect(allowlistHas(al, "item/fileChange/requestApproval", { grantRoot: "/vault/b" })).toBe(false);
  });
});

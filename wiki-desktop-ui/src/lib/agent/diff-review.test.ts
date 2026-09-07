/**
 * diff-review.test.ts —— 纯函数层单测（node 环境，无 DOM/Tauri）
 * 覆盖：多文件切分、± 统计、reverse→apply 往返还原、路径归一、空/畸形输入
 */
import { describe, it, expect } from "vitest";
import { applyPatch } from "diff";
import {
  parseAggregatedDiff,
  countLooseLines,
  reverseFilePatch,
  resolveDiffPath,
  toDisplayPath,
  reverseApplyToCurrent,
  forwardApplyToDisk,
  summarizePatches,
} from "./diff-review";

// —— fixture：git 风格聚合 diff（两文件：修改 + 新增） ——

const MULTI_FILE_DIFF = `diff --git a/notes/todo.md b/notes/todo.md
index 1111111..2222222 100644
--- a/notes/todo.md
+++ b/notes/todo.md
@@ -1,4 +1,5 @@
 # 待办
 - 买牛奶
+- 修水管
 - 写周报
 line4
diff --git a/notes/new.md b/notes/new.md
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/notes/new.md
@@ -0,0 +1,2 @@
+# 新增
+内容
`;

describe("parseAggregatedDiff 多文件切分", () => {
  it("切出两文件并保留各自原始 patch", () => {
    const patches = parseAggregatedDiff(MULTI_FILE_DIFF);
    expect(patches).toHaveLength(2);
    expect(patches[0]).toMatchObject({ path: "notes/todo.md", kind: "change" });
    expect(patches[1]).toMatchObject({ path: "notes/new.md", kind: "create" });
    // 原始 patch 文本相互独立
    expect(patches[0].patch).toContain("todo.md");
    expect(patches[0].patch).not.toContain("new.md");
    expect(patches[1].patch).toContain("new.md");
    expect(patches[1].patch).not.toContain("todo.md");
  });

  it("逐文件 ± 统计正确", () => {
    const patches = parseAggregatedDiff(MULTI_FILE_DIFF);
    expect({ a: patches[0].adds, d: patches[0].dels }).toEqual({ a: 1, d: 0 });
    expect({ a: patches[1].adds, d: patches[1].dels }).toEqual({ a: 2, d: 0 });
  });

  it("+++ / --- 文件头不计入行数", () => {
    expect(countLooseLines("--- a/x\n+++ b/x\n+1\n-2")).toEqual({ adds: 1, dels: 1 });
  });
});

describe("reverse→apply 往返还原", () => {
  const BEFORE = "# 待办\n- 买牛奶\n- 写周报\nline4\n";
  const AFTER = "# 待办\n- 买牛奶\n- 修水管\n- 写周报\nline4\n";

  it("反向补丁应用到 after 得到 before", () => {
    const [p] = parseAggregatedDiff(MULTI_FILE_DIFF);
    const rev = reverseFilePatch(p);
    expect(rev).toBeTruthy();
    const out = applyPatch(AFTER, rev as string);
    expect(out).toBe(BEFORE);
  });

  it("reverseApplyToCurrent(after) → before；forwardApplyToDisk(before) → after", () => {
    const [p] = parseAggregatedDiff(MULTI_FILE_DIFF);
    const back = reverseApplyToCurrent(AFTER, p);
    expect(back).toEqual({ content: BEFORE });
    const fwd = forwardApplyToDisk(BEFORE, p);
    expect(fwd).toEqual({ content: AFTER });
  });

  it("reverseFilePatch 对非结构化兜底块返回 null", () => {
    const [p] = parseAggregatedDiff("随便一段不是 diff 的文字\n+但有加号行\n");
    expect(p.structured).toBeNull();
    expect(reverseFilePatch(p)).toBeNull();
  });

  it("磁盘内容漂移导致反向应用失败时返回 reason（不抛错）", () => {
    const [p] = parseAggregatedDiff(MULTI_FILE_DIFF);
    const res = reverseApplyToCurrent("完全不相关的内容\n", p);
    expect(res).toHaveProperty("reason");
  });
});

describe("resolveDiffPath 路径归一", () => {
  const ROOT = "/vault/notes-root";

  it("a/ b/ 前缀剥离后拼 vault 根", () => {
    expect(resolveDiffPath("b/notes/todo.md", ROOT)).toBe(`${ROOT}/notes/todo.md`);
    expect(resolveDiffPath("a/notes/todo.md", ROOT)).toBe(`${ROOT}/notes/todo.md`);
  });

  it("./ 前缀与裸相对路径拼 vault 根", () => {
    expect(resolveDiffPath("./notes/a.md", ROOT)).toBe(`${ROOT}/notes/a.md`);
    expect(resolveDiffPath("notes/a.md", ROOT)).toBe(`${ROOT}/notes/a.md`);
  });

  it("绝对路径原样归一（不拼根）", () => {
    expect(resolveDiffPath("/etc/hosts", ROOT)).toBe("/etc/hosts");
    expect(resolveDiffPath("/vault/notes-root/x.md", ROOT)).toBe(`${ROOT}/x.md`);
  });

  it("空路径与 /dev/null 返回空串", () => {
    expect(resolveDiffPath("", ROOT)).toBe("");
    expect(resolveDiffPath("/dev/null", ROOT)).toBe("");
  });

  it("toDisplayPath 把 vault 内绝对路径还原为相对展示", () => {
    expect(toDisplayPath(`${ROOT}/notes/todo.md`, ROOT)).toBe("notes/todo.md");
    expect(toDisplayPath("/else/where.md", ROOT)).toBe("/else/where.md");
  });
});

describe("空/畸形输入容错", () => {
  it("空串/空白返回空数组", () => {
    expect(parseAggregatedDiff("")).toEqual([]);
    expect(parseAggregatedDiff("   \n  ")).toEqual([]);
  });

  it("纯垃圾文本不抛错，兜底为原文块", () => {
    const patches = parseAggregatedDiff("not a diff at all ((((");
    expect(patches).toHaveLength(1);
    expect(patches[0].structured).toBeNull();
    expect(patches[0].patch).toContain("not a diff");
  });

  it("多块中坏块跳过、好块保留", () => {
    const mixed = `diff --git a/ok.md b/ok.md
--- a/ok.md
+++ b/ok.md
@@ -1 +1 @@
-a
+b
`;
    const patches = parseAggregatedDiff(`${mixed}\n这段只是杂散文字\n`);
    // parsePatch 能解出 ok.md 块；杂散文字不破坏好块
    expect(patches.some((p) => p.path === "ok.md")).toBe(true);
  });
});

describe("summarizePatches", () => {
  it("空组返回 null", () => {
    expect(summarizePatches([])).toBeNull();
  });

  it("累加 files/adds/dels", () => {
    const patches = parseAggregatedDiff(MULTI_FILE_DIFF);
    expect(summarizePatches(patches)).toEqual({ files: 2, adds: 3, dels: 0 });
  });
});

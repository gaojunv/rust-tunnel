import { describe, expect, it } from "vitest";
import { parseWikilinkTarget } from "./App";

describe("parseWikilinkTarget", () => {
  it("纯 key 原样返回（侧栏/搜索/图谱/报告调用方不受影响）", () => {
    expect(parseWikilinkTarget("folder/note")).toEqual(["folder/note", null]);
    expect(parseWikilinkTarget("note")).toEqual(["note", null]);
  });

  it("note#heading 拆出笔记与标题", () => {
    expect(parseWikilinkTarget("note#heading")).toEqual(["note", "heading"]);
    expect(parseWikilinkTarget("folder/note#第二章 概览")).toEqual(["folder/note", "第二章 概览"]);
  });

  it("纯锚点 #heading 返回空笔记部分（同笔记内滚动）", () => {
    expect(parseWikilinkTarget("#heading")).toEqual(["", "heading"]);
  });

  it("多级 # 保留在 heading 部分", () => {
    expect(parseWikilinkTarget("a#b#c")).toEqual(["a", "b#c"]);
  });

  it("空 heading 视为无锚点", () => {
    expect(parseWikilinkTarget("note#")).toEqual(["note", null]);
    expect(parseWikilinkTarget("note#   ")).toEqual(["note", null]);
  });

  it("heading 两端空白被 trim", () => {
    expect(parseWikilinkTarget("note#  第二章  ")).toEqual(["note", "第二章"]);
  });
});

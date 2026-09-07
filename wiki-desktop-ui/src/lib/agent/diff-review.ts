/**
 * Diff 审查纯函数层（2A'）—— 与 DOM / Tauri 无关，vitest node 可测
 *
 * 职责：把 turn 级聚合 unified diff（`turn/diff/updated` 载荷）切成单文件补丁、
 * 统计 ±、生成反向补丁、归一 diff 路径。基于 `diff` 包 v9
 * （parsePatch / reversePatch / applyPatch，libesm 实测均可用）。
 *
 * 设计约定：
 * - 每文件 diff 方向 = "由 a（变更前）→ b（变更后）"：
 *   `applyPatch(磁盘当前内容, patch)` 得到变更后内容；其反向 patch 撤销该回合改动。
 * - FilePatch.patch 总是**原始 patch 文本**（含 diff --git 头等，供"复制反向补丁"原文展示）；
 *   reverse→apply 操作在 StructuredPatch 结构上做，不再做文本 hunk 级翻转。
 * - 畸形输入一律不抛错：坏块整体跳过，能解的部分照常返回。
 */

import { parsePatch, reversePatch, applyPatch, formatPatch } from "diff";
import type { StructuredPatch } from "diff";
/**
 * 最小 POSIX 路径工具（替代 node:path）。
 * 说明：本文件所有路径在调用前已把反斜杠统一为正斜杠（见 resolveDiffPath），
 * 且仅用 `join`（join+normalize）、`normalize`（`.`/`..` 归一）、`isAbsolute`（`/` 或盘符开头）、
 * `sep`（`"/"` 字面量）。node:path 在浏览器 bundle 下被 vite 外置导致构建失败
 * （2E 集成 DiffReviewDialog 后首次进入 bundle），故以内联实现替换，行为与 POSIX 语义一致。
 */
const sep = "/";

function isAbsolute(p: string): boolean {
  return p.startsWith("/") || /^[A-Za-z]:\//.test(p);
}

/** POSIX normalize：合并多余 `/`，消解 `.`/`..`（`..` 到根即停，不上溢） */
export function posixNormalize(p: string): string {
  if (!p) return "";
  const absolute = p.startsWith("/");
  const trailing = p.endsWith("/") && p.length > 1;
  const parts = p.split("/");
  const out: string[] = [];
  for (const part of parts) {
    if (!part || part === ".") continue;
    if (part === "..") {
      if (out.length > 0) out.pop();
      continue;
    }
    out.push(part);
  }
  const joined = out.join("/");
  if (absolute) return `/${joined}${trailing && joined ? "/" : ""}`;
  return joined || (trailing ? "/" : "");
}

const normalize = posixNormalize;

/** POSIX join：拼接后归一（空段忽略） */
function join(...parts: string[]): string {
  return posixNormalize(parts.filter((s) => s !== "").join("/"));
}

// —— FilePatch：单文件可操作补丁 ——

/**
 * 解析后的单文件补丁（由聚合 diff 切出）。
 * - `kind`：change / create / delete（依据 git 扩展头，缺省 change）
 * - `patch`：**原始 patch 文本**——展示、复制、透传 applyPatch 时用
 * - `structured`：结构补丁；`structured` / `patch` 空时表示本文件无 hunk 可还原
 */
export type FilePatch = {
  /** 变更后文件路径（b/ 侧；create/rename 优先取 newFileName） */
  path: string;
  /** 变更前文件路径（a/ 侧；create 无 a/ 侧时同 path） */
  oldPath: string;
  kind: "change" | "create" | "delete";
  /** 相对/绝对未归一路径，仅诊断展示 */
  rawPath: string;
  /** 新增行数（+，hunk 行统计） */
  adds: number;
  /** 删除行数（-，hunk 行统计） */
  dels: number;
  /** 原始 patch 文本（缺 file 头的可能仅含 hunk） */
  patch: string;
  /** 结构化补丁；parsePatch 失败或无 hunk 时为 null */
  structured: StructuredPatch | null;
};

// —— 工具 ——

/** 把 StructuredPatch 系列化为 unified 文本（git 风格头尽量保留） */
function toPatchText(sp: StructuredPatch): string {
  try {
    return formatPatch(sp);
  } catch {
    return sp.hunks
      .map((h) =>
        [
          `@@ -${h.oldStart},${h.oldLines} +${h.newStart},${h.newLines} @@`,
          ...h.lines,
        ].join("\n"),
      )
      .join("\n");
  }
}

/** 按 git 扩展头判断文件种类；无法判定时按 hunks 推断 */
function inferKind(sp: StructuredPatch): FilePatch["kind"] {
  if (sp.isCreate) return "create";
  if (sp.isDelete) return "delete";
  if (sp.isRename || sp.isCopy) return sp.isRename ? "delete" : "change"; // rename 视为删除旧文件
  // 无扩展头时兜底推断
  if (!sp.oldFileName && sp.newFileName) return "create";
  if (!sp.newFileName && sp.oldFileName) return "delete";
  return "change";
}

/**
 * 剥 git 文件头的 `a/`/`b/` 前缀，返回逻辑仓库相对路径。
 * `/dev/null`（新增/删除占位）与空值返回空串。
 */
function stripGitPrefix(name: string | undefined | null): string {
  if (!name) return "";
  const s = name.replace(/\\/g, "/");
  const m = /^(?:[ab]\/)(.*)$/.exec(s);
  if (m) return m[1] as string;
  if (s === "/dev/null") return "";
  return s;
}

/** 从结构 patch 统计 ± 行数（+/- 前缀首字符判定，context/空行不计） */
function countFromHunks(sp: StructuredPatch): { adds: number; dels: number } {
  let adds = 0;
  let dels = 0;
  for (const h of sp.hunks) {
    for (const line of h.lines) {
      if (line.startsWith("+")) adds += 1;
      else if (line.startsWith("-")) dels += 1;
    }
  }
  return { adds, dels };
}

// —— 主解析：聚合 unified diff → FilePatch[] ——

/**
 * 将聚合 unified diff 切为单文件补丁（diff 包 `parsePatch` 主解析）。
 *
 * 实测 v9 行为（已固化到 .test.ts）：parsePatch 对 git dialect 切分正确但
 * **保留 `a/`/`b/` 前缀**（newFileName="b/x.md"，此处经 stripGitPrefix 剥离）；
 * 对纯垃圾文本不抛错而返回无文件名、无 hunk 的占位组（此处过滤），全部无名
 * 时整体兜底为原文单块（structured=null，调用方降级原文预览）。全程不抛错。
 *
 * 统计基于 hunk 行首字符（+++ 文件头不在 lines 内，天然排除）。
 */
/** 兜底：整块不可解析时以原文为单块返回（调用方降级原文预览；structured=null 禁写回） */
function fallbackSinglePatch(diff: string): FilePatch[] {
  const trimmed = diff.trim();
  return [
    {
      path: "",
      oldPath: "",
      kind: "change",
      rawPath: "",
      adds: countLooseLines(trimmed).adds,
      dels: countLooseLines(trimmed).dels,
      patch: diff,
      structured: null,
    },
  ];
}

export function parseAggregatedDiff(diff: string): FilePatch[] {
  if (typeof diff !== "string" || !diff.trim()) return [];
  let out: FilePatch[] = [];
  try {
    // diff 包 parsePatch 不会为空字符串/垃圾抛错，但可能返回无文件名、无 hunk 的占位组；
    // 实测它保留 a/ b/ 前缀（newFileName="b/x.md"），需在此剥离归一。
    const sps = parsePatch(diff);
    for (const sp of sps) {
      const newName = stripGitPrefix(sp.newFileName);
      const oldName = stripGitPrefix(sp.oldFileName);
      // 删除类：new 侧为 /dev/null；新增类：old 侧为 /dev/null
      const path = newName || oldName;
      if (!path) continue; // 无名占位（垃圾）块：跳过
      const { adds, dels } = countFromHunks(sp);
      out.push({
        path,
        oldPath: oldName || path,
        kind: inferKind(sp),
        // 展示用原始头名（可能带 b/ 前缀），保留原文身份
        rawPath: sp.newFileName || sp.oldFileName || path,
        adds,
        dels,
        patch: toPatchText(sp),
        structured: sp,
      });
    }
  } catch {
    // 极端畸形：不抛，走兜底
    out = [];
  }
  // 全为垃圾/空组 → 单块原文兜底（保持“畸形输入不抛错”契约）
  if (out.length === 0) return fallbackSinglePatch(diff);
  return out;
}

/** 兜底统计：宽松扫行（-/+ 前缀，排除 +++/--- 文件头）。仅供不可解析块显示用 */
export function countLooseLines(diff: string): { adds: number; dels: number } {
  let adds = 0;
  let dels = 0;
  for (const l of diff.split("\n")) {
    if (l.startsWith("+++") || l.startsWith("---")) continue;
    if (l.startsWith("+")) adds += 1;
    else if (l.startsWith("-")) dels += 1;
  }
  return { adds, dels };
}

// —— 反向 patch ——

/**
 * 生成 FilePatch 的反向 patch 文本。
 * - 有结构化补丁时走 diff 包 `reversePatch`（仅对 git 扩展头做有限可逆处理）+ formatPatch；
 *   返回 null 表示不可反向（copy from/to 反演、二进制等），由调用方降级。
 * - 无结构化补丁（parsePatch 兜底块）返回 null。
 */
export function reverseFilePatch(filePatch: FilePatch): string | null {
  if (!filePatch.structured) return null;
  try {
    const reversed = reversePatch(filePatch.structured);
    const text = formatPatch(reversed);
    return text || null;
  } catch {
    return null;
  }
}

// —— 路径归一 ——

/**
 * diff 里的路径 → vault 根下绝对路径。
 * 处理 `a/` `b/` 前缀、`./*`、相对/绝对混合；所有反斜杠转正斜杠后参与计算（win 路径容错）。
 * - 已绝对（含盘符 / 起始 `/`）→ normalize 原样返回
 * - 相对 → join(vaultRoot, p)
 * 纯 path 计算不触碰文件系统（canonicalize 交给调用方，本组件不 import fs）
 */
export function resolveDiffPath(rawPath: string, vaultRoot: string): string {
  let p = (typeof rawPath === "string" ? rawPath : "").trim();
  if (!p) return "";
  // 反斜杠统一为正斜杠（windows 风格路径、diff 内转义）
  p = p.replace(/\\/g, "/");
  // 去除 git 前后缀
  const m = /^(?:[ab]\/)(.*)$/.exec(p);
  if (m) p = m[1] as string;
  if (p.startsWith("./")) p = p.slice(2);
  if (!p || p === "/dev/null") return "";

  // 已是绝对路径：win 盘符（C:/…）或 unix（/…）
  if (isAbsolute(p) || /^[A-Za-z]:\//.test(p)) {
    // 仍可能含 a/b 前缀深度（如 "b/a/xx" 开头但绝对）——不二次剥，直接归一
    return normalize(p);
  }
  // 相对路径拼 vault 根
  const root = vaultRoot && vaultRoot.length > 0 ? normalize(vaultRoot.replace(/\\/g, "/")) : "";
  if (!root) return normalize(p); // 无 root 信息：仅归一
  // 防越出 vault：把 ".." 逐段吞掉到 root 为止
  return normalize(join(root, p));
}

/** vault 内路径 → 展示用相对路径（相对 vault 根）；越出或解析失败回退原路径 */
export function toDisplayPath(absPath: string, vaultRoot: string): string {
  if (!vaultRoot) return absPath;
  try {
    const root = normalize(vaultRoot.replace(/\\/g, "/")).replace(/\/+$/, "");
    const abs = normalize(absPath.replace(/\\/g, "/"));
    if (abs === root) return "/";
    if (abs.startsWith(root + sep) || abs.startsWith(root + "/")) {
      return abs.slice(root.length + 1);
    }
    return absPath;
  } catch {
    return absPath;
  }
}

// —— 反向应用（撤销磁盘改动）——

/**
 * 对当前磁盘内容应用反向 patch 还原到该回合改动前。
 * @param current    磁盘当前内容（fsReadFileText 已解码）
 * @param filePatch  该文件的原始补丁
 * @returns { content } 还原后文本；{ null, reason } 失败原因（降级用）
 *
 * 实现：磁盘内容 = after，应用**反向** patch → before。
 * 复用 diff 包 `applyPatch(current, reversePatch(structured))`，保持行尾语义一致，
 * 提供 fuzzFactor 1 以容忍极少数 context 漂移。
 */
export function reverseApplyToCurrent(
  current: string,
  filePatch: FilePatch,
): { content: string } | { reason: string } {
  if (!filePatch.structured) return { reason: "该补丁无法结构化解析（非 unified diff）" };
  try {
    const reversed = reversePatch(filePatch.structured);
    // 反向后的 newFileName 与磁盘文件应同名（若结构 patch 本身 rename/copy 可能不同，applyPatch 用源串不校验文件名）
    const result = applyPatch(current, reversed, { fuzzFactor: 1 });
    if (result === false) return { reason: "反向补丁无法精确应用（磁盘内容与该回合 diff 不一致）" };
    return { content: result };
  } catch (e) {
    return { reason: e instanceof Error ? e.message : String(e) };
  }
}

/** 正向应用：磁盘内容（before）+ 原 patch → after（预览"变更后"用，与 fs 读盘互为备份） */
export function forwardApplyToDisk(current: string, filePatch: FilePatch): { content: string } | { reason: string } {
  if (!filePatch.structured) return { reason: "该补丁无法结构化解析（非 unified diff）" };
  try {
    const result = applyPatch(current, filePatch.structured, { fuzzFactor: 1 });
    if (result === false) return { reason: "正向补丁无法精确应用" };
    return { content: result };
  } catch (e) {
    return { reason: e instanceof Error ? e.message : String(e) };
  }
}

// —— 汇总（供 TurnDiffBar 徽标）——

/** 把一组 FilePatch 的 ± 统计累加；空组返回 null（无 diff 不显示） */
export function summarizePatches(patches: FilePatch[]): { files: number; adds: number; dels: number } | null {
  if (!patches.length) return null;
  const adds = patches.reduce((n, p) => n + p.adds, 0);
  const dels = patches.reduce((n, p) => n + p.dels, 0);
  return { files: patches.length, adds, dels };
}

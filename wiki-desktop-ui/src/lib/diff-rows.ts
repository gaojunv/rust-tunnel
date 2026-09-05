import { diffLines } from "diff";

export type DiffRow = {
  type: "same" | "add" | "del" | "pair";
  left?: string;
  right?: string;
  leftNo?: number;
  rightNo?: number;
};

/**
 * 将左右文本按行 diff，对齐 removed+added 对为 pair 行
 */
export function buildDiffRows(localText: string, remoteText: string): DiffRow[] {
  if (localText === remoteText) {
    if (localText === "") return [];
    let lines = localText.split("\n");
    if (localText.endsWith("\n") && lines[lines.length - 1] === "") lines = lines.slice(0, -1);
    return lines.map((line, idx) => ({
      type: "same" as const,
      left: line,
      right: line,
      leftNo: idx + 1,
      rightNo: idx + 1,
    }));
  }

  function splitValue(value: string): string[] {
    const parts = value.split("\n");
    if (parts.length > 0 && parts[parts.length - 1] === "" && value.endsWith("\n")) {
      parts.pop();
    }
    return parts;
  }

  // Normalize trailing newline so "a\\nb" vs "a\\nb\\nc" is seen as append rather than
  // "b" vs "b\\n" pair — avoids newline-terminator artifacts from diffLines.
  const normalize = (s: string) => (s === "" || s.endsWith("\n") ? s : s + "\n");
  const a = normalize(localText);
  const b = normalize(remoteText);

  const changes = diffLines(a, b);
  const rows: DiffRow[] = [];
  let leftNo = 1;
  let rightNo = 1;
  let i = 0;
  while (i < changes.length) {
    const cur = changes[i];
    if (!cur.added && !cur.removed) {
      const lines = splitValue(cur.value);
      for (const line of lines) {
        rows.push({ type: "same", left: line, right: line, leftNo, rightNo });
        leftNo++;
        rightNo++;
      }
      i++;
    } else if (cur.removed) {
      const next = changes[i + 1];
      if (next && next.added) {
        const leftLines = splitValue(cur.value);
        const rightLines = splitValue(next.value);
        const n = Math.max(leftLines.length, rightLines.length);
        for (let k = 0; k < n; k++) {
          const left = leftLines[k];
          const right = rightLines[k];
          const hasLeft = k < leftLines.length;
          const hasRight = k < rightLines.length;
          if (hasLeft && hasRight) {
            rows.push({ type: "pair", left, right, leftNo, rightNo });
            leftNo++;
            rightNo++;
          } else if (hasLeft) {
            rows.push({ type: "del", left, leftNo });
            leftNo++;
          } else {
            rows.push({ type: "add", right, rightNo });
            rightNo++;
          }
        }
        i += 2;
      } else {
        const lines = splitValue(cur.value);
        for (const line of lines) {
          rows.push({ type: "del", left: line, leftNo });
          leftNo++;
        }
        i++;
      }
    } else {
      const lines = splitValue(cur.value);
      for (const line of lines) {
        rows.push({ type: "add", right: line, rightNo });
        rightNo++;
      }
      i++;
    }
  }
  return rows;
}

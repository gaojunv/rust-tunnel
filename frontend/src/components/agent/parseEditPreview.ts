import type { ToolDiff } from './types';

/**
 * Parse an approval `args_preview` string into `ToolDiff[]` if it matches the
 * edit/write preview format produced by the runner's `build_args_preview`.
 *
 * Returns `null` when the preview is not an edit preview (e.g. plain shell
 * command summary).
 *
 * Recognized formats (produced by `build_args_preview` in runner.rs):
 *
 * 1. **patch_file / edit_file**: first line is path, then `--- old` / `+++ new` pairs.
 *    Multiple edits → multiple `--- old`/`+++ new` blocks (edit_file caps at 5
 *    with a trailing `... (N more edits)` marker).
 *
 * 2. **write_file**: first line is path, then `--- new content (N shown / M total)`
 *    followed by the new content.
 */
export function parseEditPreview(args_preview: string): ToolDiff[] | null {
  if (!args_preview) return null;

  const lines = args_preview.split('\n');
  if (lines.length < 2) return null;

  // Must start with a path-like first line (no edit markers on line 0)
  const path = lines[0];
  if (!path || path.startsWith('---') || path.startsWith('+++')) return null;

  // Detect format by second-line marker
  const secondLine = lines[1];

  // --- write_file format: --- new content (N shown / M total)
  if (secondLine.startsWith('--- new content')) {
    // Everything from line 2 onward (after the marker) is the new content,
    // but may be truncated with [... truncated, N total lines]
    const contentLines = lines.slice(2);
    // Strip truncation marker if present
    let content = contentLines.join('\n');
    const truncIdx = content.lastIndexOf('\n[... truncated');
    if (truncIdx !== -1) {
      content = content.slice(0, truncIdx);
    }
    return [{ path, old_text: null, new_text: content }];
  }

  // --- edit_file / patch_file format: --- old / +++ new pairs
  if (!secondLine.startsWith('--- old')) return null;

  const diffs: ToolDiff[] = [];
  let i = 2; // start after `--- old` (line 0 = path, line 1 = --- old)

  while (i < lines.length) {
    // Skip `--- old` marker
    if (lines[i].startsWith('--- old')) i++;

    // Collect old text until `+++ new`
    const oldLines: string[] = [];
    while (i < lines.length && !lines[i].startsWith('+++ new')) {
      oldLines.push(lines[i]);
      i++;
    }
    // Skip `+++ new` marker
    if (i < lines.length) i++;

    // Collect new text until next `--- old`, trailing "... (N more edits)", or end
    const newLines: string[] = [];
    let hitTruncMarker = false;
    while (i < lines.length && !lines[i].startsWith('--- old')) {
      // edit_file truncation marker: "... (N more edits)"
      if (lines[i].startsWith('... (')) { hitTruncMarker = true; break; }
      newLines.push(lines[i]);
      i++;
    }

    diffs.push({
      path,
      old_text: oldLines.join('\n') || null,
      new_text: newLines.join('\n') || null,
    });

    // Trailing marker reached — no more diffs
    if (hitTruncMarker) break;
  }

  return diffs.length > 0 ? diffs : null;
}

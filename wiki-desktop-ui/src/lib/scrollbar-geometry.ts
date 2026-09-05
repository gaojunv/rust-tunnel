// 纯函数：overlay 滚动条几何计算（无 DOM 依赖，便于单测）
const DEFAULT_MIN_THUMB = 24;

/**
 * 计算 thumb 长度
 * - scrollSize <= clientSize 时无需滚动条，返回 0
 * - 否则 thumb = max(minThumb, clientSize * clientSize / scrollSize)
 */
export function thumbSize(clientSize: number, scrollSize: number, minThumb: number = DEFAULT_MIN_THUMB): number {
  if (!Number.isFinite(clientSize) || !Number.isFinite(scrollSize) || !Number.isFinite(minThumb)) return 0;
  if (clientSize <= 0 || scrollSize <= 0) return 0;
  if (scrollSize <= clientSize) return 0;
  const raw = (clientSize * clientSize) / scrollSize;
  const min = minThumb > 0 ? minThumb : DEFAULT_MIN_THUMB;
  return Math.max(min, raw);
}

/**
 * 把 scrollOffset 映射为 thumb 在轨道内的偏移
 */
export function thumbOffset(
  scrollOffset: number,
  clientSize: number,
  scrollSize: number,
  thumbLen: number,
): number {
  if (!Number.isFinite(scrollOffset) || !Number.isFinite(clientSize) || !Number.isFinite(scrollSize) || !Number.isFinite(thumbLen)) return 0;
  if (clientSize <= 0 || scrollSize <= 0 || thumbLen <= 0) return 0;
  if (scrollSize <= clientSize) return 0;
  const maxScroll = scrollSize - clientSize;
  const maxThumbPos = clientSize - thumbLen;
  if (maxThumbPos <= 0) return 0;
  if (maxScroll <= 0) return 0;
  // 越界 clamp
  const clampedScroll = Math.max(0, Math.min(maxScroll, scrollOffset));
  return (clampedScroll / maxScroll) * maxThumbPos;
}

/**
 * 拖拽反映射：thumb 位置 -> scrollOffset
 */
export function scrollOffsetFromThumb(
  thumbPos: number,
  clientSize: number,
  scrollSize: number,
  thumbLen: number,
): number {
  if (!Number.isFinite(thumbPos) || !Number.isFinite(clientSize) || !Number.isFinite(scrollSize) || !Number.isFinite(thumbLen)) return 0;
  if (clientSize <= 0 || scrollSize <= 0 || thumbLen <= 0) return 0;
  if (scrollSize <= clientSize) return 0;
  const maxScroll = scrollSize - clientSize;
  const maxThumbPos = clientSize - thumbLen;
  if (maxThumbPos <= 0) return 0;
  if (maxScroll <= 0) return 0;
  const clampedThumb = Math.max(0, Math.min(maxThumbPos, thumbPos));
  return (clampedThumb / maxThumbPos) * maxScroll;
}

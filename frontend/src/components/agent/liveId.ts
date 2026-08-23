let liveItemSeq = 0;
export function nextLiveItemId(): string {
  liveItemSeq += 1;
  return `live-${liveItemSeq}`;
}
export function resetLiveItemSeq(): void {
  liveItemSeq = 0;
}

// frontend/src/utils/format.ts

export const formatBytes = (bytes: number): string => {
  if (bytes === 0) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  const value = bytes / Math.pow(k, i);
  // 精度随量级递减，避免 "683.59 KB" 这类过长标签挤压坐标轴
  const decimals = value >= 100 ? 0 : value >= 10 ? 1 : 2;
  return parseFloat(value.toFixed(decimals)) + ' ' + sizes[i];
};

export const formatBps = (bytesPerSec: number): string =>
  formatBytes(bytesPerSec) + '/s';

export const formatMs = (value: number): string => {
  if (value < 10) return value.toFixed(1) + ' ms';
  if (value < 100) return value.toFixed(0) + ' ms';
  return Math.round(value).toString() + ' ms';
};

export const formatPercent = (value: number): string =>
  (value * 100).toFixed(1) + '%';

// 展示时间戳：ISO → 本地字符串，空/非法返回 '-'。
export const formatDateTime = (iso: string | null | undefined): string => {
  if (!iso) return '-';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return String(iso);
  return d.toLocaleString();
};

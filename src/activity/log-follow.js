export const LOG_BOTTOM_THRESHOLD_PX = 8;

export function isLogAtBottom({ scrollHeight, clientHeight, scrollTop }) {
  return scrollHeight - clientHeight - scrollTop <= LOG_BOTTOM_THRESHOLD_PX;
}

export function compensateTrimmedScrollTop(previousTop, previousHeight, nextHeight) {
  const removedHeight = Math.max(0, previousHeight - nextHeight);
  return Math.max(0, previousTop - removedHeight);
}

export function shouldTrimLog(entryCount, maxEntries = 10_000) {
  return entryCount >= maxEntries;
}

export function logEntryAction(item, status) {
  if (status === 'processing') return item ? 'ignore' : 'append';
  if (!item) return 'append';
  return item.completed ? 'ignore' : 'promote';
}

export function progressPercent(ratio) {
  return Math.round(ratio * 100);
}

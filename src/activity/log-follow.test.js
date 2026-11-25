import { describe, expect, it } from 'vitest';
import {
  compensateTrimmedScrollTop,
  isLogAtBottom,
  logEntryAction,
  progressPercent,
  shouldTrimLog,
} from './log-follow.js';

describe('activity log auto-follow', () => {
  it('treats the final eight pixels as the bottom', () => {
    expect(isLogAtBottom({ scrollHeight: 1000, clientHeight: 300, scrollTop: 692 })).toBe(true);
    expect(isLogAtBottom({ scrollHeight: 1000, clientHeight: 300, scrollTop: 691 })).toBe(false);
  });

  it('resumes when the user scrolls all the way back down', () => {
    expect(isLogAtBottom({ scrollHeight: 1000, clientHeight: 300, scrollTop: 400 })).toBe(false);
    expect(isLogAtBottom({ scrollHeight: 1000, clientHeight: 300, scrollTop: 700 })).toBe(true);
  });

  it('keeps the same visible content when old rows are trimmed', () => {
    expect(compensateTrimmedScrollTop(500, 2000, 1960)).toBe(460);
    expect(compensateTrimmedScrollTop(20, 2000, 1960)).toBe(0);
  });

  it('trims before the 10,001st row and compensates the reader position', () => {
    expect(shouldTrimLog(9_999)).toBe(false);
    expect(shouldTrimLog(10_000)).toBe(true);
    expect(compensateTrimmedScrollTop(500, 20_000, 19_998)).toBe(498);
  });

  it('promotes one processing row exactly once when completion arrives', () => {
    expect(logEntryAction(undefined, 'processing')).toBe('append');
    expect(logEntryAction({ completed: false }, 'done')).toBe('promote');
    expect(logEntryAction({ completed: true }, 'done')).toBe('ignore');
  });

  it('converts active-file ratios to the displayed progress percentage', () => {
    expect(progressPercent(0)).toBe(0);
    expect(progressPercent(0.504)).toBe(50);
    expect(progressPercent(1)).toBe(100);
  });
});

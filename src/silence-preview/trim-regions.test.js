import { describe, it, expect } from 'vitest';
import { createLatestTracker, parseTrimInput, removedRegions } from './trim-regions.js';

describe('parseTrimInput', () => {
  it('returns the fallback for empty or non-numeric text', () => {
    expect(parseTrimInput('', -100, -1, -80)).toBe(-80);
    expect(parseTrimInput('abc', -100, -1, -80)).toBe(-80);
  });

  it('clamps dB values into -100..-1', () => {
    expect(parseTrimInput('0', -100, -1, -80)).toBe(-1);
    expect(parseTrimInput('-150', -100, -1, -80)).toBe(-100);
    expect(parseTrimInput('-60', -100, -1, -80)).toBe(-60);
  });

  it('clamps millisecond values into 10..2000', () => {
    expect(parseTrimInput('5', 10, 2000, 50)).toBe(10);
    expect(parseTrimInput('9000', 10, 2000, 50)).toBe(2000);
    expect(parseTrimInput('120', 10, 2000, 50)).toBe(120);
  });
});

describe('removedRegions', () => {
  const trim = (keepStart, keepEnd, cutHead, cutTail, duration = 10) => ({
    outcome: 'trim',
    keepStart,
    keepEnd,
    cutHead,
    cutTail,
    duration,
  });

  it('returns the head and tail outside the kept range for a trim', () => {
    expect(removedRegions(trim(1, 8, true, true), 10)).toEqual([
      [0, 1],
      [8, 10],
    ]);
  });

  it('drops sides the backend does not cut', () => {
    expect(removedRegions(trim(0, 8, false, true), 10)).toEqual([[8, 10]]);
    expect(removedRegions(trim(2, 10, true, false), 10)).toEqual([[0, 2]]);
  });

  it('draws a real cut even when it is shorter than 0.05 s', () => {
    expect(removedRegions(trim(0.03, 9.97, true, true), 10)).toEqual([
      [0, 0.03],
      [9.97, 10],
    ]);
  });

  it('does not draw a side that is not cut despite a duration mismatch', () => {
    expect(removedRegions(trim(1, 9.98, true, false, 9.98), 10)).toEqual([[0, 1]]);
  });

  it('falls back to the backend duration without a waveform duration', () => {
    expect(removedRegions(trim(1, 8, true, true, 9), 0)).toEqual([
      [0, 1],
      [8, 9],
    ]);
  });

  it('returns nothing when nothing is removed', () => {
    for (const outcome of ['none', 'allSilent', 'failed']) {
      expect(
        removedRegions({ ...trim(0, 10, false, false), outcome }, 10),
      ).toEqual([]);
    }
  });
});

describe('createLatestTracker', () => {
  it('ignores a rejected older request while a newer one is pending', () => {
    const tracker = createLatestTracker();
    const older = tracker.begin();
    let status = 'analyzing';
    const newer = tracker.begin();
    // The older request now fails because it was superseded.
    if (tracker.isLatest(older)) status = 'error';
    expect(status).toBe('analyzing');
    expect(tracker.isLatest(newer)).toBe(true);
  });
});

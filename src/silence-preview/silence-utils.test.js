import { describe, it, expect } from 'vitest';
import { detectSilence } from './silence-utils.js';

const TOTAL = 10.0; // テスト用の仮想総長 (秒)

// RMS 値を生成するヘルパー
// quiet: 0.0 (無音), loud: 1.0 (音あり)
function makeRms(pattern, n = 100) {
  // pattern: 配列 [{from, to, value}]
  const rms = new Array(n).fill(1.0);
  for (const { from, to, value } of pattern) {
    const si = Math.round(from / TOTAL * n);
    const ei = Math.round(to / TOTAL * n);
    for (let i = si; i < ei; i++) rms[i] = value;
  }
  return rms;
}

describe('detectSilence', () => {
  it('空の配列は空を返す', () => {
    expect(detectSilence([], -80, 0.05, TOTAL)).toEqual([]);
  });

  it('無音なし → 空を返す', () => {
    const rms = new Array(100).fill(1.0); // 全部大きい値
    expect(detectSilence(rms, -80, 0.05, TOTAL)).toEqual([]);
  });

  it('全部無音 → 先頭1件のみ返す（先頭=末尾が同じ）', () => {
    const rms = new Array(100).fill(0.0); // 全部無音
    const result = detectSilence(rms, -80, 0.05, TOTAL);
    // 先頭から始まり末尾まで続く領域が1件
    expect(result.length).toBe(1);
    expect(result[0][0]).toBeCloseTo(0, 1);
    expect(result[0][1]).toBeCloseTo(TOTAL, 1);
  });

  it('先頭無音のみ検出', () => {
    // 0〜1秒 が無音、残りは有音
    const rms = makeRms([{ from: 0, to: 1.0, value: 0.0 }]);
    const result = detectSilence(rms, -80, 0.05, TOTAL);
    expect(result.length).toBe(1);
    expect(result[0][0]).toBeCloseTo(0, 1);
    expect(result[0][1]).toBeCloseTo(1.0, 0);
  });

  it('末尾無音のみ検出', () => {
    // 9〜10秒 が無音
    const rms = makeRms([{ from: 9.0, to: 10.0, value: 0.0 }]);
    const result = detectSilence(rms, -80, 0.05, TOTAL);
    expect(result.length).toBe(1);
    expect(result[0][1]).toBeCloseTo(TOTAL, 1);
  });

  it('先頭と末尾の両方に無音 → 2件返す', () => {
    const rms = makeRms([
      { from: 0, to: 0.5, value: 0.0 },
      { from: 9.5, to: 10.0, value: 0.0 },
    ]);
    const result = detectSilence(rms, -80, 0.05, TOTAL);
    expect(result.length).toBe(2);
    expect(result[0][0]).toBeCloseTo(0, 1);
    expect(result[1][1]).toBeCloseTo(TOTAL, 1);
  });

  it('中間の無音は無視される', () => {
    // 5〜6秒 の中間無音
    const rms = makeRms([{ from: 5.0, to: 6.0, value: 0.0 }]);
    const result = detectSilence(rms, -80, 0.05, TOTAL);
    expect(result).toEqual([]);
  });

  it('最小長より短い無音は無視される', () => {
    // 先頭 0.01秒 (< minDuration=0.5秒)
    const rms = makeRms([{ from: 0, to: 0.01, value: 0.0 }]);
    const result = detectSilence(rms, -80, 0.5, TOTAL);
    expect(result).toEqual([]);
  });

  it('dB 閾値より大きいRMSは有音と判定', () => {
    // -60dB = 0.001, RMS=0.01 は -60dB より大きいので有音
    const rms = new Array(100).fill(0.01);
    const result = detectSilence(rms, -60, 0.05, TOTAL);
    expect(result).toEqual([]);
  });

  it('dB 閾値以下のRMSは無音と判定', () => {
    // -60dB ≈ 0.001, RMS=0.0001 は無音
    const rms = makeRms([{ from: 0, to: 1.0, value: 0.0001 }]);
    const result = detectSilence(rms, -60, 0.05, TOTAL);
    expect(result.length).toBe(1);
  });
});

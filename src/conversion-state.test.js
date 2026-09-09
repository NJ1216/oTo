/**
 * conversion-state.test.js
 *
 * main.js の変換状態管理ロジックをインラインでシミュレートして検証する。
 * 対象: jobCancelled フラグが残ったまま再変換した際に setState('standby') が
 *       呼ばれなくなるレースコンディション。
 */

import { describe, it, expect, vi } from 'vitest';

// ---- main.js の状態管理ロジックをそのまま移植したシミュレーション ----

function makeConversionState({ setState, showCompletionToast, saveLastSettings }) {
  let isProcessing = false;
  let activeJobId = null;
  let jobCancelled = false;

  // ---- 修正前 (startConversion で jobCancelled をリセットしない) ----
  function startConversionBuggy() {
    if (isProcessing) return false;
    isProcessing = true;
    // jobCancelled = false; ← 修正前はこの行がない
    activeJobId = 'job-' + Math.random();
    setState('processing');
    return true;
  }

  // ---- 修正後 (startConversion で jobCancelled をリセットする) ----
  function startConversionFixed() {
    if (isProcessing) return false;
    isProcessing = true;
    jobCancelled = false; // ← 修正点
    activeJobId = 'job-' + Math.random();
    setState('processing');
    return true;
  }

  // conversion_complete リスナーの処理（main.js と同一）
  function handleConversionComplete(payload) {
    if (jobCancelled) {
      jobCancelled = false;
      return;
    }
    isProcessing = false;
    activeJobId = null;
    setState('standby');
    showCompletionToast(payload.successCount, payload.errorCount, payload.results);
    saveLastSettings();
  }

  // btn-cancel-job / cancelAllViaOverwrite 相当
  function cancelJob() {
    jobCancelled = true;
    activeJobId = null;
    isProcessing = false;
    setState('standby');
  }

  // テスト用ゲッター
  const getState = () => ({ isProcessing, activeJobId, jobCancelled });

  return { startConversionBuggy, startConversionFixed, handleConversionComplete, cancelJob, getState };
}

const makePayload = (success = 1, error = 0) => ({
  successCount: success, errorCount: error, results: [],
});

// ---- テストスイート ----

describe('正常フロー: 変換完了で standby に戻る', () => {
  it('変換完了後に setState("standby") が呼ばれる', () => {
    const setState = vi.fn();
    const { startConversionFixed, handleConversionComplete } = makeConversionState({
      setState, showCompletionToast: vi.fn(), saveLastSettings: vi.fn(),
    });

    startConversionFixed();
    handleConversionComplete(makePayload());

    expect(setState).toHaveBeenCalledWith('processing');
    expect(setState).toHaveBeenLastCalledWith('standby');
  });
});

describe('バグ再現: キャンセル後の再変換で standby に戻れない（修正前）', () => {
  it('修正前: jobCancelled=true のまま再変換すると conversion_complete で return してしまう', () => {
    const setState = vi.fn();
    const { startConversionBuggy, handleConversionComplete, cancelJob } = makeConversionState({
      setState, showCompletionToast: vi.fn(), saveLastSettings: vi.fn(),
    });

    // 1回目: 変換開始 → キャンセル
    startConversionBuggy();
    cancelJob(); // jobCancelled = true

    // 2回目: 再変換（jobCancelled がリセットされない）
    startConversionBuggy();
    handleConversionComplete(makePayload()); // jobCancelled=true → return → setState('standby') が呼ばれない

    // 最後の setState 呼び出しは cancelJob の 'standby' のはず（2回目の完了では呼ばれない）
    const calls = setState.mock.calls.map(c => c[0]);
    // processing, standby(cancel), processing の3回のみ（4回目の standby が来ない）
    expect(calls).toEqual(['processing', 'standby', 'processing']);
    // → バグ: 最後が 'standby' でない
    expect(calls.at(-1)).not.toBe('standby');
  });
});

describe('修正後: キャンセル後の再変換でも正常に standby に戻る', () => {
  it('修正後: startConversion で jobCancelled をリセットするため conversion_complete が正常処理される', () => {
    const setState = vi.fn();
    const { startConversionFixed, handleConversionComplete, cancelJob } = makeConversionState({
      setState, showCompletionToast: vi.fn(), saveLastSettings: vi.fn(),
    });

    // 1回目: 変換開始 → キャンセル
    startConversionFixed();
    cancelJob(); // jobCancelled = true

    // 2回目: 再変換（jobCancelled が false にリセットされる）
    startConversionFixed();
    handleConversionComplete(makePayload());

    const calls = setState.mock.calls.map(c => c[0]);
    // processing, standby(cancel), processing, standby(complete) の4回
    expect(calls).toEqual(['processing', 'standby', 'processing', 'standby']);
    expect(calls.at(-1)).toBe('standby');
  });

  it('修正後: 連続キャンセルを繰り返しても毎回正常完了できる', () => {
    const setState = vi.fn();
    const { startConversionFixed, handleConversionComplete, cancelJob } = makeConversionState({
      setState, showCompletionToast: vi.fn(), saveLastSettings: vi.fn(),
    });

    for (let i = 0; i < 3; i++) {
      startConversionFixed();
      cancelJob();
    }

    startConversionFixed();
    handleConversionComplete(makePayload());

    const calls = setState.mock.calls.map(c => c[0]);
    expect(calls.at(-1)).toBe('standby');
  });

  it('修正後: showCompletionToast が呼ばれる（キャンセル後の再変換）', () => {
    const setState = vi.fn();
    const showCompletionToast = vi.fn();
    const { startConversionFixed, handleConversionComplete, cancelJob } = makeConversionState({
      setState, showCompletionToast, saveLastSettings: vi.fn(),
    });

    startConversionFixed();
    cancelJob();
    startConversionFixed();
    handleConversionComplete(makePayload(3, 1));

    expect(showCompletionToast).toHaveBeenCalledOnce();
    expect(showCompletionToast).toHaveBeenCalledWith(3, 1, []);
  });
});

describe('キャンセル中に conversion_complete が届いた場合', () => {
  it('jobCancelled=true 時は setState("standby") を呼ばない（キャンセル側が既に呼んでいる）', () => {
    const setState = vi.fn();
    const { startConversionFixed, handleConversionComplete, cancelJob } = makeConversionState({
      setState, showCompletionToast: vi.fn(), saveLastSettings: vi.fn(),
    });

    startConversionFixed();
    cancelJob(); // setState('standby') 呼び出し済み

    // Rust側から遅れて conversion_complete が届く
    handleConversionComplete(makePayload());

    // cancelJob 時の 'standby' のみ（重複呼び出しなし）
    const standbyCalls = setState.mock.calls.filter(c => c[0] === 'standby');
    expect(standbyCalls).toHaveLength(1);
  });

  it('jobCancelled フラグが conversion_complete 後にリセットされる', () => {
    const setState = vi.fn();
    const { startConversionFixed, handleConversionComplete, cancelJob, getState } = makeConversionState({
      setState, showCompletionToast: vi.fn(), saveLastSettings: vi.fn(),
    });

    startConversionFixed();
    cancelJob();
    handleConversionComplete(makePayload()); // jobCancelled → false にリセット

    expect(getState().jobCancelled).toBe(false);
  });
});

import { describe, expect, it, vi } from 'vitest';

function makeQueueState(invoke) {
  let isProcessing = false;
  let sessionId = null;
  let mode = 'encode';
  let format = 'mp3';

  return {
    setProfile(nextMode, nextFormat) {
      mode = nextMode;
      format = nextFormat;
    },
    async drop(paths, settings) {
      if (!isProcessing) {
        isProcessing = true;
        sessionId = 'session-1';
      }
      const snapshot = structuredClone(settings);
      await invoke('convert_files', {
        jobId: sessionId,
        request: { paths, mode, format },
        settingsSnapshot: snapshot,
      });
    },
    complete() {
      isProcessing = false;
      sessionId = null;
    },
    state() {
      return { isProcessing, sessionId };
    },
  };
}

describe('single-instance conversion queue', () => {
  it('accepts another drop with the same session ID while conversion is active', async () => {
    const invoke = vi.fn().mockResolvedValue({});
    const queue = makeQueueState(invoke);
    await queue.drop(['/one.wav'], { nameConflict: 'auto_rename' });
    await queue.drop(['/two.wav'], { nameConflict: 'auto_rename' });

    expect(invoke).toHaveBeenCalledTimes(2);
    expect(invoke.mock.calls[0][1].jobId).toBe('session-1');
    expect(invoke.mock.calls[1][1].jobId).toBe('session-1');
    expect(queue.state().isProcessing).toBe(true);
  });

  it('captures mode, format, and settings independently for every drop', async () => {
    const invoke = vi.fn().mockResolvedValue({});
    const queue = makeQueueState(invoke);
    const firstSettings = { nameConflict: 'force_overwrite', mp3Bitrate: 192 };
    await queue.drop(['/one.wav'], firstSettings);
    firstSettings.mp3Bitrate = 320;
    queue.setProfile('decode', 'aiff');
    await queue.drop(['/two.m4a'], firstSettings);

    expect(invoke.mock.calls[0][1].request).toMatchObject({ mode: 'encode', format: 'mp3' });
    expect(invoke.mock.calls[0][1].settingsSnapshot.mp3Bitrate).toBe(192);
    expect(invoke.mock.calls[1][1].request).toMatchObject({ mode: 'decode', format: 'aiff' });
    expect(invoke.mock.calls[1][1].settingsSnapshot.mp3Bitrate).toBe(320);
  });

  it('only returns to idle when the session completion event arrives', async () => {
    const queue = makeQueueState(vi.fn().mockResolvedValue({}));
    await queue.drop(['/one.wav'], {});
    await queue.drop(['/two.wav'], {});
    expect(queue.state().isProcessing).toBe(true);
    queue.complete();
    expect(queue.state()).toEqual({ isProcessing: false, sessionId: null });
  });
});

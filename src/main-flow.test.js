import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(), events: new Map(), setState: vi.fn(), setProgress: vi.fn(),
  onCloseRequested: vi.fn(),
}));
vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(async (name, handler) => { mocks.events.set(name, handler); }),
}));
vi.mock('@tauri-apps/api/webviewWindow', () => ({
  getCurrentWebviewWindow: () => ({ setTitle: vi.fn(), onCloseRequested: mocks.onCloseRequested }),
}));
vi.mock('./svg-controller.js', () => ({
  initSVGController: vi.fn(async () => {}), setState: mocks.setState,
  setFormat: vi.fn(), setMode: vi.fn(), setProgress: mocks.setProgress,
}));
vi.mock('./i18n/index.js', () => ({ initI18n: vi.fn(async () => {}), t: (key) => key }));

let elements;
let requests;
function emit(name, payload) { return mocks.events.get(name)({ payload }); }
function drop(path) { emit('tauri://drag-drop', { paths: [path] }); }
function complete(jobId) {
  emit('conversion_complete', { jobId, successCount: 1, errorCount: 0, skippedCount: 0, results: [] });
}

beforeEach(async () => {
  vi.resetModules();
  vi.clearAllMocks();
  mocks.events.clear();
  elements = new Map();
  requests = [];
  vi.stubGlobal('document', {
    addEventListener: vi.fn(), querySelectorAll: () => [],
    getElementById(id) {
      if (!elements.has(id)) elements.set(id, {
        style: {}, classList: { add: vi.fn(), remove: vi.fn(), contains: () => true },
        listeners: new Map(), addEventListener(name, fn) { this.listeners.set(name, fn); },
      });
      return elements.get(id);
    },
  });
  mocks.invoke.mockImplementation((command, args) => {
    if (command === 'get_settings') return Promise.resolve({ lastMode: 'encode', lastFormat: 'mp3', language: 'en' });
    if (command === 'is_silence_preview_visible') return Promise.resolve(false);
    if (command === 'convert_files') return new Promise((resolve, reject) => requests.push({ args, resolve, reject }));
    return Promise.resolve();
  });
  await import('./main.js');
  await vi.waitFor(() => expect(mocks.onCloseRequested).toHaveBeenCalledOnce());
});

afterEach(() => { vi.unstubAllGlobals(); });

describe('production conversion handlers', () => {
  it('keeps a newer session progressing when an old enqueue fails late', async () => {
    drop('/first.wav');
    await elements.get('btn-cancel-job').listeners.get('click')();
    drop('/second.wav');
    const current = requests[1].args.jobId;
    expect(current).not.toBe(requests[0].args.jobId);
    requests[0].reject(new Error('late old request failure'));
    await vi.waitFor(() => expect(mocks.setState).toHaveBeenLastCalledWith('processing'));
    await Promise.resolve();
    emit('progress', { jobId: current, percent: 58 });
    expect(mocks.setProgress).toHaveBeenLastCalledWith(58);
    requests[1].resolve({});
    complete(current);
    expect(mocks.setState).toHaveBeenLastCalledWith('standby');
  });

  it('ignores a late completion of a cancelled job and still finishes the next one', async () => {
    drop('/first.wav');
    const cancelled = requests[0].args.jobId;
    requests[0].resolve({});
    await elements.get('btn-cancel-job').listeners.get('click')();
    const calls = mocks.setState.mock.calls.length;
    const toastBefore = elements.get('toast').textContent;
    complete(cancelled);
    expect(mocks.setState).toHaveBeenCalledTimes(calls);
    expect(elements.get('toast').textContent).toBe(toastBefore);
    drop('/second.wav');
    requests[1].resolve({});
    complete(requests[1].args.jobId);
    expect(mocks.setState).toHaveBeenLastCalledWith('standby');
  });

  it('ignores duplicate completion and completion from an earlier session', async () => {
    drop('/first.wav');
    const first = requests[0].args.jobId;
    requests[0].resolve({});
    complete(first);
    const calls = mocks.setState.mock.calls.length;
    complete(first);
    expect(mocks.setState).toHaveBeenCalledTimes(calls);
    drop('/second.wav');
    complete(first);
    expect(mocks.setState).toHaveBeenLastCalledWith('processing');
    emit('progress', { jobId: requests[1].args.jobId, percent: 71 });
    expect(mocks.setProgress).toHaveBeenLastCalledWith(71);
    requests[1].resolve({});
    complete(requests[1].args.jobId);
    expect(mocks.setState).toHaveBeenLastCalledWith('standby');
  });

  it('queues another drop in the active session and handles a normal initial rejection', async () => {
    drop('/first.wav');
    requests[0].reject(new Error('initial rejection'));
    await vi.waitFor(() => expect(mocks.setState).toHaveBeenLastCalledWith('standby'));
    drop('/second.wav');
    drop('/third.wav');
    expect(requests[1].args.jobId).toBe(requests[2].args.jobId);
    requests[1].resolve({});
    requests[2].resolve({});
    complete(requests[1].args.jobId);
    expect(mocks.setState).toHaveBeenLastCalledWith('standby');
  });

  it('keeps an accepted batch active if the first enqueue in the same session fails', async () => {
    drop('/first.wav');
    drop('/second.wav');
    const jobId = requests[1].args.jobId;
    requests[1].resolve({});
    requests[0].reject(new Error('first enqueue failed'));
    await Promise.resolve();
    await Promise.resolve();
    emit('progress', { jobId, percent: 42 });
    expect(mocks.setProgress).toHaveBeenLastCalledWith(42);
    complete(jobId);
  });

  it('returns to standby when every pending enqueue is rejected', async () => {
    drop('/first.wav');
    drop('/second.wav');
    requests[0].reject(new Error('first rejected'));
    requests[1].reject(new Error('second rejected'));
    await vi.waitFor(() => expect(mocks.setState).toHaveBeenLastCalledWith('standby'));
  });

  it('reports files converted without trimming', async () => {
    drop('/first.wav');
    requests[0].resolve({});
    const jobId = requests[0].args.jobId;
    await emit('conversion_complete', {
      jobId, successCount: 3, errorCount: 0, skippedCount: 0,
      results: [
        { success: true, trimNotApplied: 'allSilent' },
        { success: true, trimNotApplied: 'analysisFailed' },
        { success: true },
      ],
    });
    const toast = elements.get('toast');
    expect(toast.textContent).toContain('toast.trimNotApplied');
    expect(toast.className).toContain('success');
  });

  it('uses the new quality profile after cancellation and restart', async () => {
    await emit('settings_updated', { language: 'en', mp3Preset: '192' });
    drop('/first.wav');
    requests[0].resolve({});
    await elements.get('btn-cancel-job').listeners.get('click')();
    await emit('settings_updated', { language: 'en', mp3Preset: '320' });
    drop('/second.wav');
    requests[1].resolve({});
    complete(requests[1].args.jobId);
    expect(elements.get('toast').textContent).toContain('320kbps');
  });
});

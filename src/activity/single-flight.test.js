import { expect, it, vi } from 'vitest';
import { singleFlight } from './single-flight.js';

it('keeps one request in flight and permits the next poll after completion', async () => {
  let resolve;
  const task = vi.fn(() => new Promise(done => { resolve = done; }));
  const poll = singleFlight(task);
  const first = poll('cursor-1');
  await poll('cursor-1');
  await poll('cursor-1');
  expect(task).toHaveBeenCalledTimes(1);
  resolve('snapshot-1');
  expect(await first).toBe('snapshot-1');
  const next = poll('cursor-2');
  expect(task).toHaveBeenLastCalledWith('cursor-2');
  resolve('snapshot-2');
  expect(await next).toBe('snapshot-2');
});

it('releases the pending flag on failures, including synchronous exceptions', async () => {
  const task = vi.fn()
    .mockRejectedValueOnce(new Error('IPC failed'))
    .mockImplementationOnce(() => { throw new Error('render failed'); })
    .mockResolvedValue('recovered');
  const poll = singleFlight(task);
  await expect(poll()).rejects.toThrow('IPC failed');
  await expect(poll()).rejects.toThrow('render failed');
  await expect(poll()).resolves.toBe('recovered');
});

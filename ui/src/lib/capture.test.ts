import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { FlowSummary } from './types';

const api = vi.hoisted(() => ({
  fetchAllFlows: vi.fn(),
  fetchHeld: vi.fn(),
  clearFlows: vi.fn(),
  wsUrl: () => 'ws://test/api/ws',
}));
vi.mock('./api', () => api);

import { Capture } from './capture.svelte';

class FakeWebSocket {
  static last: FakeWebSocket;
  onopen: (() => void) | null = null;
  onmessage: ((m: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  constructor(public url: string) {
    FakeWebSocket.last = this;
  }
  close() {}
  emit(ev: unknown) {
    this.onmessage?.({ data: JSON.stringify(ev) });
  }
}

let frames: Array<() => void> = [];
const flushFrames = () => {
  const run = frames;
  frames = [];
  run.forEach((f) => f());
};

function flow(id: number): FlowSummary {
  return {
    id,
    method: 'GET',
    url: `http://example.test/${id}`,
    started_at: '2026-01-01T00:00:00Z',
  } as FlowSummary;
}

const ids = (c: Capture) => c.rows.map((r) => r.id);
const settle = () => new Promise((r) => setTimeout(r, 0));

/** Opens the socket, which triggers the initial reload. */
async function started(snapshot: FlowSummary[]) {
  api.fetchAllFlows.mockResolvedValue(snapshot);
  const c = new Capture();
  c.start();
  FakeWebSocket.last.onopen?.();
  await settle();
  flushFrames();
  return c;
}

beforeEach(() => {
  frames = [];
  api.fetchHeld.mockResolvedValue([]);
  api.clearFlows.mockResolvedValue(undefined);
  vi.stubGlobal('WebSocket', FakeWebSocket);
  vi.stubGlobal('requestAnimationFrame', (cb: () => void) => frames.push(cb));
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.clearAllMocks();
});

describe('Capture while paused', () => {
  it('shows the snapshot after a reload, and holds only live events', async () => {
    const c = await started([flow(1)]);
    c.setPaused(true);
    FakeWebSocket.last.emit({ type: 'flow', flow: flow(2) });
    expect(c.pendingCount).toBe(1);

    api.fetchAllFlows.mockResolvedValue([flow(1), flow(2), flow(3)]);
    FakeWebSocket.last.emit({ type: 'resync' });
    await settle();
    flushFrames();

    expect(ids(c)).toEqual([1, 2, 3]);
    expect(c.pendingCount).toBe(0);

    FakeWebSocket.last.emit({ type: 'flow', flow: flow(4) });
    flushFrames();
    expect(ids(c)).toEqual([1, 2, 3]);
    expect(c.pendingCount).toBe(1);

    c.setPaused(false);
    flushFrames();
    expect(ids(c)).toEqual([1, 2, 3, 4]);
  });

  it('keeps the snapshot when reconnecting while paused', async () => {
    const c = await started([]);
    c.setPaused(true);
    api.fetchAllFlows.mockResolvedValue([flow(1), flow(2)]);
    FakeWebSocket.last.onopen?.();
    await settle();
    flushFrames();
    c.setPaused(false);
    flushFrames();
    expect(ids(c)).toEqual([1, 2]);
  });

  it('a clear discards events held before it, not after it', async () => {
    const c = await started([flow(1)]);
    c.setPaused(true);
    FakeWebSocket.last.emit({ type: 'flow', flow: flow(2) });
    await c.clear();
    FakeWebSocket.last.emit({ type: 'flow', flow: flow(3) });
    flushFrames();

    expect(ids(c)).toEqual([]);
    expect(c.pendingCount).toBe(1);

    c.setPaused(false);
    flushFrames();
    expect(ids(c)).toEqual([3]);
  });
});

describe('Capture sources', () => {
  const from = (id: number, client: string) => ({ ...flow(id), client }) as FlowSummary;

  it('empties the source list when flows are cleared', async () => {
    const c = await started([from(1, '10.0.0.1'), from(2, '10.0.0.2')]);
    expect(c.clients).toEqual(['10.0.0.1', '10.0.0.2']);

    await c.clear();
    flushFrames();

    expect(c.clients).toEqual([]);
  });

  it('lists only clients seen after a clear', async () => {
    const c = await started([from(1, '10.0.0.1'), from(2, '10.0.0.2')]);
    await c.clear();
    FakeWebSocket.last.emit({ type: 'flow', flow: from(3, '10.0.0.3') });
    flushFrames();

    expect(c.clients).toEqual(['10.0.0.3']);
  });

  it('drops clients that are gone after a reload snapshot', async () => {
    const c = await started([from(1, '10.0.0.1')]);
    api.fetchAllFlows.mockResolvedValue([from(2, '10.0.0.2')]);
    FakeWebSocket.last.emit({ type: 'resync' });
    await settle();
    flushFrames();

    expect(c.clients).toEqual(['10.0.0.2']);
  });

  it('drops clients whose rows were all evicted', async () => {
    const old = Array.from({ length: 2_000 }, (_, i) => from(i + 1, '10.0.0.1'));
    const recent = Array.from({ length: 8_001 }, (_, i) => from(i + 2_001, '10.0.0.2'));
    const c = await started([...old, ...recent]);

    expect(c.clients).toEqual(['10.0.0.2']);
  });
});

describe('Capture row cap', () => {
  it('evicts the oldest rows beyond the server-side store size', async () => {
    const c = await started(Array.from({ length: 10_001 }, (_, i) => flow(i + 1)));
    expect(c.rows.length).toBeLessThanOrEqual(10_000);
    expect(c.rows.at(-1)?.id).toBe(10_001);
  });
});

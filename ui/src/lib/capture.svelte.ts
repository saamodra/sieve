import { fetchAllFlows, fetchHeld, clearFlows, wsUrl } from './api';
import type { FlowSummary, HeldRequest, WsEvent } from './types';

/** A summary plus precomputed fields for fast filtering and rendering. */
export interface Row extends FlowSummary {
  ts: number;
  search: string;
}

const MAX_ROWS = 50_000;
const EVICT_BATCH = 2_000;

function toRow(f: FlowSummary): Row {
  return {
    ...f,
    ts: Date.parse(f.started_at),
    search: `${f.method} ${f.url} ${f.status ?? ''} ${f.content_type ?? ''} ${f.error ?? ''}`.toLowerCase(),
  };
}

/**
 * Live flow list. Rows live in a plain array (a deep reactive proxy over tens
 * of thousands of objects is slow); `version` is bumped once per animation
 * frame so views recompute at most once per frame however fast flows arrive.
 */
export class Capture {
  rows: Row[] = [];
  version = $state(0);
  paused = $state(false);
  pendingCount = $state(0);
  connected = $state(false);
  clients = $state<string[]>([]);
  /** Requests held at a breakpoint, waiting for Continue or Abort. */
  held = $state<HeldRequest[]>([]);

  #index = new Map<number, number>();
  #clientSet = new Set<string>();
  #incoming: WsEvent[] = [];
  #held: WsEvent[] = [];
  #scheduled = false;
  #ws: WebSocket | null = null;
  #retry: ReturnType<typeof setTimeout> | undefined;
  #closed = false;

  start() {
    this.#closed = false;
    this.#connect();
  }

  stop() {
    this.#closed = true;
    clearTimeout(this.#retry);
    this.#ws?.close();
  }

  #connect() {
    const ws = new WebSocket(wsUrl());
    this.#ws = ws;
    ws.onopen = () => {
      this.connected = true;
      void this.#reload();
    };
    ws.onmessage = (m) => this.#push(JSON.parse(m.data as string) as WsEvent);
    ws.onclose = () => {
      this.connected = false;
      if (!this.#closed) this.#retry = setTimeout(() => this.#connect(), 1000);
    };
  }

  async #reload() {
    try {
      const all = await fetchAllFlows();
      this.#push({ type: 'clear' });
      for (const flow of all) this.#push({ type: 'flow', flow });
      this.held = await fetchHeld().catch(() => []);
    } catch (e) {
      console.error('loading flows failed', e);
    }
  }

  #push(ev: WsEvent) {
    if (ev.type === 'resync') {
      void this.#reload();
      return;
    }
    if (ev.type === 'breakpoint') {
      this.held = [...this.held.filter((h) => h.id !== ev.request.id), ev.request];
      return;
    }
    if (ev.type === 'breakpoint_released') {
      this.held = this.held.filter((h) => h.id !== ev.id);
      return;
    }
    if (this.paused && ev.type === 'flow') {
      this.#held.push(ev);
      this.pendingCount = this.#held.length;
      return;
    }
    this.#incoming.push(ev);
    if (!this.#scheduled) {
      this.#scheduled = true;
      requestAnimationFrame(() => this.#flush());
    }
  }

  #flush() {
    this.#scheduled = false;
    const events = this.#incoming;
    this.#incoming = [];
    let newClient = false;
    for (const ev of events) {
      if (ev.type === 'clear') {
        this.rows = [];
        this.#index.clear();
        this.#held = [];
        this.pendingCount = 0;
        continue;
      }
      if (ev.type !== 'flow') continue;
      const row = toRow(ev.flow);
      const at = this.#index.get(row.id);
      if (at === undefined) {
        this.#index.set(row.id, this.rows.length);
        this.rows.push(row);
      } else {
        this.rows[at] = row;
      }
      if (row.client && !this.#clientSet.has(row.client)) {
        this.#clientSet.add(row.client);
        newClient = true;
      }
    }
    if (this.rows.length > MAX_ROWS) {
      this.rows = this.rows.slice(EVICT_BATCH);
      this.#index = new Map(this.rows.map((r, i) => [r.id, i]));
    }
    if (newClient) this.clients = [...this.#clientSet].sort();
    this.version++;
  }

  setPaused(paused: boolean) {
    this.paused = paused;
    if (!paused) {
      const held = this.#held;
      this.#held = [];
      this.pendingCount = 0;
      for (const ev of held) this.#push(ev);
    }
  }

  async clear() {
    this.#push({ type: 'clear' });
    await clearFlows();
  }
}

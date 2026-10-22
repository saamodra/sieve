import { fetchAllFlows, fetchHeld, clearFlows, wsUrl } from './api';
import type { FlowSummary, HeldRequest, WsEvent } from './types';

/** A summary plus precomputed fields for fast filtering and rendering. */
export interface Row extends FlowSummary {
  ts: number;
  search: string;
}

/** Matches `DEFAULT_MAX_FLOWS` in crates/sieve-ui/src/store.rs: a reload never returns more. */
const MAX_ROWS = 10_000;
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
  #queued: WsEvent[] = [];
  #reloading = false;
  #reloadAgain = false;
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

  /**
   * Replaces local state with a server snapshot. Live events that arrive while
   * the snapshot is being fetched are queued and replayed on top of it, so a
   * newer live update is never overwritten by an older snapshot row. Only one
   * reload runs at a time; requests made meanwhile coalesce into one follow-up.
   *
   * The snapshot is applied even while paused: it replaces the visible rows and
   * discards anything held before it (the snapshot already covers those). Only
   * live events that arrive after it are held until resume.
   */
  async #reload() {
    if (this.#reloading) {
      this.#reloadAgain = true;
      return;
    }
    this.#reloading = true;
    try {
      do {
        this.#reloadAgain = false;
        let snapshot: FlowSummary[] | null = null;
        let held: HeldRequest[] | null = null;
        try {
          snapshot = await fetchAllFlows();
          held = await fetchHeld().catch(() => null);
        } catch (e) {
          console.error('loading flows failed', e);
        }
        const queued = this.#queued;
        this.#queued = [];
        if (snapshot) {
          this.#dispatch({ type: 'clear' });
          for (const flow of snapshot) this.#enqueue({ type: 'flow', flow });
        }
        if (held) this.held = held;
        for (const ev of queued) this.#dispatch(ev);
      } while (this.#reloadAgain);
    } finally {
      this.#reloading = false;
    }
  }

  #push(ev: WsEvent) {
    if (ev.type === 'resync') {
      void this.#reload();
      return;
    }
    if (this.#reloading) {
      this.#queued.push(ev);
      return;
    }
    this.#dispatch(ev);
  }

  #dispatch(ev: WsEvent) {
    if (ev.type === 'breakpoint') {
      this.held = [...this.held.filter((h) => h.id !== ev.request.id), ev.request];
      return;
    }
    if (ev.type === 'breakpoint_released') {
      this.held = this.held.filter((h) => h.id !== ev.id);
      return;
    }
    if (ev.type === 'clear') {
      // Drop only what was held before the clear; later events stay held.
      this.#held = [];
      this.pendingCount = 0;
    } else if (this.paused && ev.type === 'flow') {
      this.#held.push(ev);
      this.pendingCount = this.#held.length;
      return;
    }
    this.#enqueue(ev);
  }

  /** Queues an event for the next animation frame, bypassing pause. */
  #enqueue(ev: WsEvent) {
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
    let clientsChanged = false;
    for (const ev of events) {
      if (ev.type === 'clear') {
        this.rows = [];
        this.#index.clear();
        this.#clientSet.clear();
        clientsChanged = true;
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
        clientsChanged = true;
      }
    }
    if (this.rows.length > MAX_ROWS) {
      this.rows = this.rows.slice(EVICT_BATCH);
      this.#index = new Map(this.rows.map((r, i) => [r.id, i]));
      // Evicted rows may have been the only ones from a client.
      const remaining = new Set<string>();
      for (const r of this.rows) if (r.client) remaining.add(r.client);
      if (remaining.size !== this.#clientSet.size) clientsChanged = true;
      this.#clientSet = remaining;
    }
    if (clientsChanged) this.clients = [...this.#clientSet].sort();
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

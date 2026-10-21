import type { FlowDetail, FlowSummary } from './types';

const PAGE = 5000;

async function ok(res: Response): Promise<Response> {
  if (!res.ok) throw new Error(`${res.url}: ${res.status} ${res.statusText}`);
  return res;
}

/** Pages through every stored flow, oldest first. */
export async function fetchAllFlows(): Promise<FlowSummary[]> {
  const out: FlowSummary[] = [];
  for (;;) {
    const res = await ok(await fetch(`/api/flows?offset=${out.length}&limit=${PAGE}`));
    const page: { total: number; flows: FlowSummary[] } = await res.json();
    out.push(...page.flows);
    if (page.flows.length === 0 || out.length >= page.total) return out;
  }
}

export async function fetchDetail(id: number): Promise<FlowDetail> {
  return (await ok(await fetch(`/api/flows/${id}`))).json();
}

export async function fetchCurl(id: number): Promise<string> {
  return (await ok(await fetch(`/api/flows/${id}/curl`))).text();
}

export async function clearFlows(): Promise<void> {
  await ok(await fetch('/api/flows', { method: 'DELETE' }));
}

export const harUrl = (id: number) => `/api/flows/${id}/har`;

export function wsUrl(): string {
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
  return `${scheme}://${location.host}/api/ws`;
}

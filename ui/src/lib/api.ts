import type { FlowDetail, FlowSummary, HeldRequest, RequestEdit, Rule } from './types';

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

export async function fetchRules(): Promise<{ rules: Rule[]; path: string | null }> {
  return (await ok(await fetch('/api/rules'))).json();
}

/** Saves the rule set; rejects with the server's message when a rule is invalid. */
export async function saveRules(rules: Rule[]): Promise<Rule[]> {
  const res = await fetch('/api/rules', {
    method: 'PUT',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ rules }),
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error ?? `${res.status} ${res.statusText}`);
  return data.rules;
}

export async function fetchHeld(): Promise<HeldRequest[]> {
  return (await ok(await fetch('/api/breakpoints'))).json().then((d) => d.held);
}

/** Throws the server's message, e.g. when the request was already released. */
async function okOrMessage(res: Response): Promise<void> {
  if (res.ok) return;
  const data = await res.json().catch(() => ({}));
  throw new Error(data.error ?? `${res.status} ${res.statusText}`);
}

export async function continueHeld(id: number, edit: RequestEdit): Promise<void> {
  await okOrMessage(
    await fetch(`/api/breakpoints/${id}/continue`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(edit),
    }),
  );
}

export async function abortHeld(id: number): Promise<void> {
  await okOrMessage(await fetch(`/api/breakpoints/${id}/abort`, { method: 'POST' }));
}

export type FlowKind = 'http' | 'web_socket' | 'passthrough';

export interface FlowSummary {
  id: number;
  started_at: string;
  client: string | null;
  kind: FlowKind;
  method: string;
  url: string;
  host: string;
  path: string;
  status: number | null;
  size: number | null;
  duration_ms: number | null;
  content_type: string | null;
  error: string | null;
}

export type Headers = [string, string][];

export interface BodyDetail {
  size: number;
  truncated: boolean;
  content_type: string | null;
  content_encoding: string | null;
  captured: number;
  base64: string;
  text: string | null;
}

export interface FlowDetail {
  summary: FlowSummary;
  request: {
    method: string;
    url: string;
    http_version: string;
    headers: Headers;
    body: BodyDetail;
  };
  response: {
    status: number;
    http_version: string;
    headers: Headers;
    body: BodyDetail;
  } | null;
  error: string | null;
  timing: { wait_ms: number | null; total_ms: number | null };
}

export type WsEvent =
  | { type: 'flow'; flow: FlowSummary }
  | { type: 'clear' }
  | { type: 'resync' };

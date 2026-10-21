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
  | { type: 'resync' }
  | { type: 'breakpoint'; request: HeldRequest }
  | { type: 'breakpoint_released'; id: number; outcome: string };

export interface Mock {
  status: number;
  headers: Record<string, string>;
  body: string | null;
  body_file: string | null;
}

export interface Rule {
  id: string;
  name: string;
  enabled: boolean;
  match: {
    host: string | null;
    path: string | null;
    path_regex: string | null;
    method: string | null;
    header: { name: string; value: string | null } | null;
  };
  action: {
    block: boolean;
    mock: Mock | null;
    map_remote: string | null;
    set_request_headers: Record<string, string>;
    remove_request_headers: string[];
    set_response_headers: Record<string, string>;
    remove_response_headers: string[];
    response_status: number | null;
    response_body: string | null;
    delay_ms: number | null;
    breakpoint: boolean;
  };
}

export interface HeldRequest {
  id: number;
  method: string;
  url: string;
  headers: Headers;
  body: string | null;
  body_size: number;
  timeout_secs: number;
}

export interface RequestEdit {
  method: string;
  url: string;
  headers: Headers;
  body?: string;
}

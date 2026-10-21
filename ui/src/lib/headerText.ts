import type { Headers } from './types';

/** One `Name: value` per line. */
export function headersToText(headers: Headers): string {
  return headers.map(([n, v]) => `${n}: ${v}`).join('\n');
}

export function textToHeaders(text: string): Headers {
  const out: Headers = [];
  for (const line of text.split('\n')) {
    const at = line.indexOf(':');
    if (at <= 0) continue;
    out.push([line.slice(0, at).trim(), line.slice(at + 1).trim()]);
  }
  return out;
}

export function mapToText(map: Record<string, string>): string {
  return headersToText(Object.entries(map));
}

export function textToMap(text: string): Record<string, string> {
  return Object.fromEntries(textToHeaders(text));
}

export function listToText(list: string[]): string {
  return list.join(', ');
}

export function textToList(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter(Boolean);
}

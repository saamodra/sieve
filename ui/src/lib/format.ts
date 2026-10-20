export function formatSize(n: number | null): string {
  if (n === null) return '';
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

export function formatDuration(ms: number | null): string {
  if (ms === null) return '';
  if (ms < 1000) return `${Math.round(ms)} ms`;
  return `${(ms / 1000).toFixed(2)} s`;
}

const pad = (n: number, w = 2) => String(n).padStart(w, '0');

export function formatTime(ts: number): string {
  const d = new Date(ts);
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3)}`;
}

export function statusClass(status: number | null, error: string | null): string {
  if (error) return 's-err';
  if (status === null) return 's-pending';
  if (status >= 500) return 's-5xx';
  if (status >= 400) return 's-4xx';
  if (status >= 300) return 's-3xx';
  if (status >= 200) return 's-2xx';
  return 's-1xx';
}

export function decodeBase64(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** Classic `offset  hex bytes  |ascii|` dump, capped to keep the DOM small. */
export function hexDump(bytes: Uint8Array, limit = 64 * 1024): string {
  const lines: string[] = [];
  const end = Math.min(bytes.length, limit);
  for (let off = 0; off < end; off += 16) {
    const chunk = bytes.subarray(off, Math.min(off + 16, end));
    let hex = '';
    let ascii = '';
    for (let i = 0; i < 16; i++) {
      const b = chunk[i];
      hex += b === undefined ? '   ' : b.toString(16).padStart(2, '0') + ' ';
      if (i === 7) hex += ' ';
      if (b !== undefined) ascii += b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : '.';
    }
    lines.push(`${off.toString(16).padStart(8, '0')}  ${hex} |${ascii}|`);
  }
  if (bytes.length > limit) lines.push(`… ${bytes.length - limit} more bytes not shown`);
  return lines.join('\n');
}

export function prettyJson(text: string): string | null {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return null;
  }
}

export async function copyText(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    return;
  } catch {
    // navigator.clipboard needs a secure context; fall back for http on a LAN.
  }
  const ta = document.createElement('textarea');
  ta.value = text;
  ta.style.position = 'fixed';
  ta.style.opacity = '0';
  document.body.append(ta);
  ta.select();
  document.execCommand('copy');
  ta.remove();
}

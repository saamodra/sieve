<script lang="ts">
  import { onMount } from 'svelte';
  import { fetchRules, saveRules } from './api';
  import { listToText, mapToText, textToList, textToMap } from './headerText';
  import type { Rule } from './types';

  let { onclose }: { onclose: () => void } = $props();

  /** A rule with its map and list fields as editable text. */
  interface Draft {
    id: string;
    name: string;
    enabled: boolean;
    host: string;
    path: string;
    pathRegex: string;
    method: string;
    headerName: string;
    headerValue: string;
    block: boolean;
    breakpoint: boolean;
    mapRemote: string;
    delayMs: string;
    setRequestHeaders: string;
    removeRequestHeaders: string;
    responseStatus: string;
    responseBody: string;
    setResponseHeaders: string;
    removeResponseHeaders: string;
    mocked: boolean;
    mockStatus: string;
    mockHeaders: string;
    mockBody: string;
    mockBodyFile: string;
  }

  let drafts = $state<Draft[]>([]);
  let path = $state<string | null>(null);
  let error = $state('');
  let status = $state('');
  let loading = $state(true);

  const orNull = (s: string) => (s.trim() ? s.trim() : null);

  function toDraft(r: Rule): Draft {
    return {
      id: r.id,
      name: r.name,
      enabled: r.enabled,
      host: r.match.host ?? '',
      path: r.match.path ?? '',
      pathRegex: r.match.path_regex ?? '',
      method: r.match.method ?? '',
      headerName: r.match.header?.name ?? '',
      headerValue: r.match.header?.value ?? '',
      block: r.action.block,
      breakpoint: r.action.breakpoint,
      mapRemote: r.action.map_remote ?? '',
      delayMs: r.action.delay_ms?.toString() ?? '',
      setRequestHeaders: mapToText(r.action.set_request_headers),
      removeRequestHeaders: listToText(r.action.remove_request_headers),
      responseStatus: r.action.response_status?.toString() ?? '',
      responseBody: r.action.response_body ?? '',
      setResponseHeaders: mapToText(r.action.set_response_headers),
      removeResponseHeaders: listToText(r.action.remove_response_headers),
      mocked: r.action.mock !== null,
      mockStatus: (r.action.mock?.status ?? 200).toString(),
      mockHeaders: mapToText(r.action.mock?.headers ?? {}),
      mockBody: r.action.mock?.body ?? '',
      mockBodyFile: r.action.mock?.body_file ?? '',
    };
  }

  function toRule(d: Draft): Rule {
    const num = (s: string) => (s.trim() && !Number.isNaN(Number(s)) ? Number(s) : null);
    return {
      id: d.id,
      name: d.name,
      enabled: d.enabled,
      match: {
        host: orNull(d.host),
        path: orNull(d.path),
        path_regex: orNull(d.pathRegex),
        method: orNull(d.method),
        header: d.headerName.trim() ? { name: d.headerName.trim(), value: orNull(d.headerValue) } : null,
      },
      action: {
        block: d.block,
        breakpoint: d.breakpoint,
        map_remote: orNull(d.mapRemote),
        delay_ms: num(d.delayMs),
        set_request_headers: textToMap(d.setRequestHeaders),
        remove_request_headers: textToList(d.removeRequestHeaders),
        response_status: num(d.responseStatus),
        response_body: d.responseBody === '' ? null : d.responseBody,
        set_response_headers: textToMap(d.setResponseHeaders),
        remove_response_headers: textToList(d.removeResponseHeaders),
        mock: d.mocked
          ? {
              status: num(d.mockStatus) ?? 200,
              headers: textToMap(d.mockHeaders),
              body: d.mockBody === '' ? null : d.mockBody,
              body_file: orNull(d.mockBodyFile),
            }
          : null,
      },
    };
  }

  const blank = (): Rule => ({
    id: '',
    name: 'New rule',
    enabled: true,
    match: { host: null, path: null, path_regex: null, method: null, header: null },
    action: {
      block: false,
      mock: null,
      map_remote: null,
      set_request_headers: {},
      remove_request_headers: [],
      set_response_headers: {},
      remove_response_headers: [],
      response_status: null,
      response_body: null,
      delay_ms: null,
      breakpoint: false,
    },
  });

  async function load() {
    loading = true;
    try {
      const data = await fetchRules();
      drafts = data.rules.map(toDraft);
      path = data.path;
      error = '';
    } catch (e) {
      error = String(e);
    }
    loading = false;
  }

  async function save() {
    status = '';
    try {
      drafts = (await saveRules(drafts.map(toRule))).map(toDraft);
      error = '';
      status = 'Saved';
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  function add() {
    drafts.push(toDraft(blank()));
  }

  function remove(i: number) {
    drafts.splice(i, 1);
  }

  function move(i: number, by: number) {
    const j = i + by;
    if (j < 0 || j >= drafts.length) return;
    const [item] = drafts.splice(i, 1);
    drafts.splice(j, 0, item!);
  }

  /** Sets the initial open state only; a bound `open` is reset by every keystroke re-render. */
  function openOnce(node: HTMLDetailsElement, open: boolean) {
    node.open = open;
  }

  onMount(load);
</script>

<aside class="panel" aria-label="Rules">
  <header>
    <strong>Rules</strong>
    {#if path}<span class="dim path" title={path}>{path}</span>{/if}
    <span class="spacer"></span>
    <button onclick={add}>Add rule</button>
    <button class="active" onclick={save}>Save</button>
    <button onclick={onclose}>Close</button>
  </header>
  {#if error}<p class="error" role="alert">{error}</p>{/if}
  {#if status}<p class="ok">{status}. Rules apply to the next request.</p>{/if}
  <div class="list">
    {#if loading}
      <p class="dim pad">Loading</p>
    {:else if drafts.length === 0}
      <p class="dim pad">
        No rules. Rules are checked in order for every request; matching rules combine. The rules file is
        also watched, so editing it on disk works too.
      </p>
    {/if}
    {#each drafts as d, i (i)}
      <details class="rule" use:openOnce={!d.id}>
        <summary>
          <input type="checkbox" bind:checked={d.enabled} aria-label="Enabled" />
          <input class="name" bind:value={d.name} aria-label="Rule name" />
          <span class="dim">{d.host || '*'} {d.path}</span>
          <span class="spacer"></span>
          <button type="button" onclick={() => move(i, -1)} aria-label="Move up">↑</button>
          <button type="button" onclick={() => move(i, 1)} aria-label="Move down">↓</button>
          <button type="button" onclick={() => remove(i)}>Delete</button>
        </summary>

        <fieldset>
          <legend>Match (all set fields must match; * is a wildcard)</legend>
          <label>Host <input bind:value={d.host} placeholder="api.prod.com or *.example.com" /></label>
          <label>Path <input bind:value={d.path} placeholder="/v1/*" /></label>
          <label>Path regex <input bind:value={d.pathRegex} placeholder="^/users/\d+$" /></label>
          <label>Method <input bind:value={d.method} placeholder="GET" /></label>
          <label>Header <input bind:value={d.headerName} placeholder="name" /></label>
          <label>Header value <input bind:value={d.headerValue} placeholder="optional, wildcard" /></label>
        </fieldset>

        <fieldset>
          <legend>Action</legend>
          <label class="check"><input type="checkbox" bind:checked={d.breakpoint} /> Breakpoint: hold the request for editing</label>
          <label class="check"><input type="checkbox" bind:checked={d.block} /> Block: answer 403</label>
          <label>Map remote <input bind:value={d.mapRemote} placeholder="localhost:3000 or https://staging.example.com" /></label>
          <label>Delay (ms) <input bind:value={d.delayMs} inputmode="numeric" /></label>
          <label>Set request headers<textarea rows="2" bind:value={d.setRequestHeaders} placeholder="Name: value"></textarea></label>
          <label>Remove request headers <input bind:value={d.removeRequestHeaders} placeholder="comma separated" /></label>
          <label>Response status <input bind:value={d.responseStatus} inputmode="numeric" /></label>
          <label>Set response headers<textarea rows="2" bind:value={d.setResponseHeaders} placeholder="Name: value"></textarea></label>
          <label>Remove response headers <input bind:value={d.removeResponseHeaders} placeholder="comma separated" /></label>
          <label>Replace response body<textarea rows="3" bind:value={d.responseBody}></textarea></label>
        </fieldset>

        <fieldset>
          <legend>Mock response (skips the server)</legend>
          <label class="check"><input type="checkbox" bind:checked={d.mocked} /> Answer with a mock</label>
          {#if d.mocked}
            <label>Status <input bind:value={d.mockStatus} inputmode="numeric" /></label>
            <label>Headers<textarea rows="2" bind:value={d.mockHeaders} placeholder="Name: value"></textarea></label>
            <label>Body<textarea rows="3" bind:value={d.mockBody}></textarea></label>
            <label>Body file <input bind:value={d.mockBodyFile} placeholder="relative to the rules file; overrides body" /></label>
          {/if}
        </fieldset>
      </details>
    {/each}
  </div>
</aside>

<style>
  .panel {
    position: fixed;
    right: 0;
    top: 0;
    bottom: 0;
    width: min(620px, 100vw);
    background: var(--bg-2);
    border-left: 1px solid var(--line);
    display: flex;
    flex-direction: column;
    z-index: 10;
  }
  header {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 8px;
    border-bottom: 1px solid var(--line);
  }
  .path {
    max-width: 220px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .spacer {
    flex: 1;
  }
  .dim {
    color: var(--fg-dim);
  }
  .pad {
    padding: 12px;
  }
  .error {
    margin: 0;
    padding: 6px 10px;
    color: var(--bad);
  }
  .ok {
    margin: 0;
    padding: 6px 10px;
    color: var(--ok);
  }
  .list {
    overflow: auto;
    flex: 1;
  }
  .rule {
    border-bottom: 1px solid var(--line);
    padding: 6px 8px;
  }
  summary {
    display: flex;
    align-items: center;
    gap: 6px;
    cursor: pointer;
  }
  .name {
    width: 180px;
  }
  fieldset {
    border: 1px solid var(--line);
    border-radius: 4px;
    margin: 8px 0 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  legend {
    color: var(--fg-dim);
    padding: 0 4px;
  }
  label {
    display: grid;
    grid-template-columns: 150px 1fr;
    align-items: start;
    gap: 6px;
    color: var(--fg-dim);
  }
  label.check {
    display: flex;
    align-items: center;
    gap: 6px;
    color: var(--fg);
  }
  textarea {
    font-family: var(--mono);
    background: var(--bg);
    border: 1px solid var(--line);
    border-radius: 4px;
    color: var(--fg);
    padding: 4px 6px;
    resize: vertical;
  }
</style>

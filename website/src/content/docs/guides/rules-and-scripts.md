---
title: Rules and scripts
description: Rewrite, mock, block, and pause traffic with rules and Rhai scripts.
---

## Rules

Rules can:

- rewrite headers, body, or status
- mock responses
- map remote hosts
- add delays
- block requests

They live in `rules.toml` (or `rules.json`) in the config directory. sieve watches the file, so edits apply right away. You can also edit rules from the web UI.

### When several rules match

Rules are checked in file order and the first one wins:

- `mock`, `map_remote`, `response_status`, `response_body`, and `delay_ms` come from the first matching rule that sets them. Delays do not add up.
- `block` and `breakpoint` are on if any matching rule turns them on.
- Header rules are decided per header name (case-insensitive). If an earlier rule sets or removes a header, later rules cannot change it. Other header names from later rules still apply.

## Breakpoints

Pause a request, edit it in the UI, then continue or abort.

## Scripts

For custom request handling, write [Rhai](https://rhai.rs) scripts and put them in the `scripts/` subdirectory of the config directory.

## Finding the config directory

`sieve ca path` prints the CA file paths, which are in the same directory:

```sh
sieve ca path
```

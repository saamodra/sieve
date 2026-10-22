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

## Breakpoints

Pause a request, edit it in the UI, then continue or abort.

## Scripts

For custom request handling, write [Rhai](https://rhai.rs) scripts and put them in the `scripts/` subdirectory of the config directory. sieve watches that directory, so adding, editing, or removing a `.rhai` file applies right away, with no restart. If a script has a syntax error, sieve logs it and keeps running the previous set of scripts until you fix it.

## Finding the config directory

`sieve ca path` prints the CA file paths, which are in the same directory:

```sh
sieve ca path
```

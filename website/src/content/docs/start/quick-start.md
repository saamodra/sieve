---
title: Quick start
description: Run sieve and send your first request through it.
---

Start the proxy and the web UI:

```sh
sieve start
```

| Service | Default address         |
| ------- | ----------------------- |
| Proxy   | `127.0.0.1:8730`        |
| Web UI  | `http://localhost:8731` |

## Check that it works

Export the root CA, then send a request through the proxy:

```sh
sieve ca export --format pem -o sieve-ca.pem
curl -x http://127.0.0.1:8730 --cacert sieve-ca.pem https://example.com
```

The request appears in the web UI at `http://localhost:8731`.

## Flags for `sieve start`

| Flag                 | Purpose                                     |
| -------------------- | ------------------------------------------- |
| `--listen`           | Proxy address                               |
| `--ui-listen`        | Web UI address                              |
| `--passthrough HOST` | Tunnel a host without intercepting it       |
| `--upstream-ca PEM`  | Trust an extra root CA for upstream servers |
| `--max-body-bytes N` | Body size limit per flow (default 5 MiB)    |

`--passthrough` and `--upstream-ca` can be repeated. `--passthrough '*.example.com'` matches every subdomain.

## Next

Point a device at sieve: see [Devices](/devices/overview/).

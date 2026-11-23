# OryonPilot transport limits

Upstream: tiny_http 0.12.0, https://github.com/tiny-http/tiny-http,
commit `212b1c45852fef2093dc1374875a9393c55eb4b9` from the downloaded crate's
`.cargo_vcs_info.json`. Sources were copied from the already locked Cargo cache.
The MIT and Apache-2.0 license files are retained. No new registry dependencies.

The upstream public API does not expose accepted sockets, connection quotas or
body-read deadlines. `recv_timeout` only waits for a parsed request. Its header
reader and internal task pool are unbounded. A handler-only thread pool therefore
does not solve slow headers, slow body readers, or excessive accepted clients.

This local patch adds an **opt-in** `Server::http_with_limits` and `ServerLimits`:

- At most the specified number of accepted connections; excess sockets are
  closed before parser work is scheduled. An RAII permit stays with both stream
  halves, including a queued request/response, and is released on final drop.
- Read and write I/O share a fixed deadline measured at accept. Before each
  socket I/O the remaining time is installed as its timeout; incoming bytes do
  not extend the deadline. Operations attempted after it fail immediately.
- An aggregate request-line/header limit and a raw incoming-byte allowance also
  bound chunk-size parsing and ignored body draining.
- Limited connections allow **one request**, return `Connection: close`, and
  never grow a pipeline of queued requests. Browsers reconnect normally.
- Internal parser workers are capped at the connection limit; reservations occur
  before thread creation. Legacy constructors retain their existing behavior.
- Minor upstream unused-import/dead-code warnings are resolved or annotated for
  the Windows host. `fetch_update` is deliberately retained for Rust 1.80 support.

OryonPilot opts into 8 connection permits, a 5-second I/O deadline, 16 KiB headers,
256 KiB raw received bytes and 4 fixed application workers. Its API separately
limits decoded bodies to 64 KiB, rejects ambiguous/truncated/non-UTF-8 bodies,
and caps CSV export buffering at 16 MiB. The connection deadline includes body
transfer and response writes; a request queued in application code is rejected
on its next I/O after the deadline. This does not cancel a stuck application
mutex or filesystem operation; those belong to the daemon liveness watchdog.
TLS is deliberately unsupported for the limited constructor; the production
listener is loopback plain HTTP. Legacy TLS constructors are unchanged.

Regression coverage is in `daemon/src/api/tests_transport.rs`: concurrent normal
requests during slow bodies, a continuously dripping body, bounded slow-header
connections and permit reuse, oversized headers/chunked bodies, incomplete and
ambiguous framing, valid chunked JSON, pipeline closure, blocked response writes,
concurrent config commits, diagnostics authentication and non-loopback refusal.
Existing API authentication, Host/Origin and actuator-gate tests remain active.

Unchanged upstream source files are retained to keep the fork auditable. Rebase
or remove this patch when upstream offers equivalent limits; do not silently
switch production back to `Server::http`.

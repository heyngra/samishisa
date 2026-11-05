# Prototype results (Phase 1 go/no-go)

Raspberry Pi 4 (aarch64, 4 cores, shared with other services), 2026-11-05. `proto/bench.sh 10 50`:
oha on the same machine, 50 connections, 2s warm-up + 10s, `GET /hello/Ada?shout=yes` (params + query validated).

| Server | req/s | p50 | p99 | RSS under load |
|---|---:|---:|---:|---:|
| Rust, fresh instance per request, 1 thread | 4,067 | 11.27 ms | 27.44 ms | 8 MB |
| Rust, pooled instances, 1 thread | **15,142** | 3.28 ms | 4.99 ms | 8 MB |
| Rust, pooled instances, 2 threads | **23,068** | 2.05 ms | 6.06 ms | 9 MB |

- Binary: 13.2 MiB (stripped, no LTO yet), needs glibc >= 2.34.
- Startup: 1.3 s the first time (compiling the 195 KB component to 739 KB native code), **8 ms** afterwards (cached native code).
- Sandbox: an infinite loop is cut at the 2 s deadline (504); a memory bomb hits the 64 MB cap (500, instance discarded); the server keeps serving.
- A fresh instance per request costs ~180 µs here (WASI context + instantiation), so per-request isolation is not the default.
  Pooled instances isolate extensions from each other and from the server, but one extension's instance can keep state between its own requests (like today's embedded tier).

Caveats: the prototype is thinner than the full pipeline (no rate limiter, cache, CORS or access log), its router matches in order (the real one ranks static segments above `:params`), and the load generator shares the Pi's CPU.

Verdict: **go**. Faster than the target on every axis.

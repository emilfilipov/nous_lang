# HTTP server example

A working HTTP/1.1 server written in **pure Lullaby** on top of the TCP socket
builtins. It shows the "framework module wraps the basics" pattern: request
parsing, routing, and response building live in a reusable `http` module, and a
thin entry program wires them to the socket loop.

## Files

- `http.lby` — the reusable framework module (`pub` functions):
  - `parse_i64` / `digit_value` — read numeric program arguments (there is no
    numeric-parse builtin yet, so this is done char by char).
  - `request_line` / `request_path` — parse the request line
    (`METHOD PATH VERSION`) using `split` on CRLF/LF and then on spaces.
  - `route_body` / `route_status` — the router seed: map a path to a body and a
    status (`/` → a greeting, `/health` → `ok`, anything else → `404 Not Found`).
  - `build_response` / `handle` — assemble a well-formed HTTP/1.1 response
    (`HTTP/1.1 <status>`, `Content-Length`, `Connection: close`, blank line, body).
  - `cr` / `lf` / `crlf` — CRLF/LF built from char codes, because Lullaby string
    literals do not interpret escape sequences.
- `server.lby` — the entry point. It binds a listener, serves a **bounded**
  number of requests (so it always terminates), and per connection runs
  `tcp_read` → `handle` → `tcp_write` → `tcp_shutdown` → `tcp_close`. The
  `tcp_shutdown` call gracefully closes the write half so the buffered response
  is delivered before the socket is dropped (otherwise a client can see an empty
  reply).

## Running it

The server takes two program arguments: the port and the number of requests to
serve before exiting (both default to `8080` and `1`):

```
lullaby run server.lby 8080 5
```

That serves five requests on port `8080`, then exits. In another terminal:

```
curl -v 127.0.0.1:8080/
curl -v 127.0.0.1:8080/health
curl -v 127.0.0.1:8080/nope
```

The first returns `200 OK` with `Hello from Lullaby!`, the second `200 OK` with
`ok`, and the third `404 Not Found`.

You can run it on any backend:

```
lullaby run --backend ir server.lby 8080 1
lullaby run --backend bytecode server.lby 8080 1
```

The Rust round-trip test `http_server_round_trip_on_all_backends` in
`crates/lullaby_cli/tests/cli.rs` drives this server as a real HTTP client on all
three backends and asserts the status line and body.

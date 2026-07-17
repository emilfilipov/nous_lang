use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root")
        .to_path_buf()
}

pub(crate) fn lullaby() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lullaby"))
}

pub(crate) fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

pub(crate) fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Whether `haystack` contains `needle` as a contiguous byte subslice.
pub(crate) fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[path = "cli/suite1.rs"]
mod suite1;

#[path = "cli/fmt_comments.rs"]
mod fmt_comments;

/// A fresh temp directory for a file-system test, using forward slashes so the
/// path can be embedded in a `.lby` string literal on every platform (Windows
/// accepts `/` in `std::fs` paths). The directory is recreated empty.
pub(crate) fn fs_temp_dir(test_name: &str) -> (std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("lullaby_cli_{test_name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let lby = dir.to_string_lossy().replace('\\', "/");
    (dir, lby)
}

#[test]
pub(crate) fn run_write_bytes_read_bytes_round_trip_on_all_backends() {
    // Write raw bytes, read them back, and reconstruct their numeric sum. The
    // program is deterministic and each backend runs against its own file.
    for backend in ["ast", "ir", "bytecode"] {
        let (dir, base) = fs_temp_dir(&format!("bytes_{backend}"));
        let path = format!("{base}/data.bin");
        let source = format!(
            "fn main -> i64\n    \
             let data list<byte> = list_new()\n    \
             data = push(data, byte(72))\n    \
             data = push(data, byte(105))\n    \
             data = push(data, byte(33))\n    \
             write_bytes(\"{path}\", data)\n    \
             let back list<byte> = read_bytes(\"{path}\")\n    \
             byte_val(get(back, 0)) + byte_val(get(back, 1)) + byte_val(get(back, 2)) + len(back)\n"
        );
        let prog = dir.join("prog.lby");
        std::fs::write(&prog, source).expect("write program");

        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                prog.to_str().expect("program path"),
            ])
            .output()
            .expect("run cli");
        assert!(output.status.success(), "{backend}: {output:?}");
        // 72 + 105 + 33 + 3 == 213
        assert_eq!(stdout(&output).trim(), "213", "{backend}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
pub(crate) fn run_read_lines_and_file_size_on_all_backends() {
    for backend in ["ast", "ir", "bytecode"] {
        let (dir, base) = fs_temp_dir(&format!("lines_{backend}"));
        let path = format!("{base}/notes.txt");
        // Seed the file from the harness (a `.lby` string literal cannot hold a
        // raw newline). "a\nbb\nccc" is 8 bytes and three lines.
        std::fs::write(dir.join("notes.txt"), "a\nbb\nccc").expect("seed file");
        let source = format!(
            "fn main -> i64\n    \
             let lines list<string> = read_lines(\"{path}\")\n    \
             let size i64 = file_size(\"{path}\")\n    \
             len(lines) * 100 + size\n"
        );
        let prog = dir.join("prog.lby");
        std::fs::write(&prog, source).expect("write program");

        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                prog.to_str().expect("program path"),
            ])
            .output()
            .expect("run cli");
        assert!(output.status.success(), "{backend}: {output:?}");
        // 3 lines * 100 + 8 bytes == 308
        assert_eq!(stdout(&output).trim(), "308", "{backend}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
pub(crate) fn run_directory_builtins_on_all_backends() {
    for backend in ["ast", "ir", "bytecode"] {
        let (dir, base) = fs_temp_dir(&format!("dirs_{backend}"));
        let sub = format!("{base}/nested");
        let file = format!("{sub}/one.txt");
        // Create a directory, put one file in it, list it, then tear it down.
        let source = format!(
            "fn flag b bool -> i64\n    if b\n        1\n    else\n        0\n\n\
             fn main -> i64\n    \
             make_dir(\"{sub}\")\n    \
             write_file(\"{file}\", \"x\")\n    \
             let is_d bool = is_dir(\"{sub}\")\n    \
             let is_f bool = is_file(\"{file}\")\n    \
             let entries list<string> = list_dir(\"{sub}\")\n    \
             remove_file(\"{file}\")\n    \
             remove_dir(\"{sub}\")\n    \
             let gone bool = is_dir(\"{sub}\")\n    \
             flag(is_d) * 1000 + flag(is_f) * 100 + len(entries) * 10 + flag(gone)\n"
        );
        let prog = dir.join("prog.lby");
        std::fs::write(&prog, source).expect("write program");

        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                prog.to_str().expect("program path"),
            ])
            .output()
            .expect("run cli");
        assert!(output.status.success(), "{backend}: {output:?}");
        // is_dir=1 -> 1000, is_file=1 -> 100, 1 entry -> 10, gone=false -> 0 == 1110
        assert_eq!(stdout(&output).trim(), "1110", "{backend}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
pub(crate) fn runs_socket_fixture_on_all_backends() {
    // The auto-run socket fixture is deterministic: `tcp_connect("127.0.0.1", 1)`
    // is a guaranteed connection refusal (port 1 is virtually always closed), so
    // the `match` takes the `err` arm and returns `1` on every backend without any
    // external server or real I/O.
    let fixture = workspace_root().join("tests/fixtures/valid/run_socket.lby");
    for backend in ["ast", "ir", "bytecode"] {
        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");

        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(stdout(&output).trim(), "1", "{backend} result");
    }
}

#[test]
pub(crate) fn tcp_client_round_trip_on_all_backends() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    // A real TCP round-trip driven from the test as the SERVER. The Lullaby
    // program is the client: it connects, writes a request, reads the reply, and
    // returns the reply length. The Rust listener replies "pong!" (5 bytes) to
    // every accepted connection, once per backend.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let port = listener.local_addr().expect("addr").port();

    let server = std::thread::spawn(move || {
        for _ in 0..3 {
            let (mut stream, _addr) = listener.accept().expect("accept");
            let mut buffer = [0u8; 64];
            let _read = stream.read(&mut buffer).expect("server read");
            stream.write_all(b"pong!").expect("server write");
            stream.flush().expect("server flush");
        }
    });

    let program = format!(
        "fn main -> i64\n    \
         let outcome result<Socket, string> = tcp_connect(\"127.0.0.1\", {port})\n    \
         match outcome\n        \
         ok(conn) -> handle(conn)\n        \
         err(message) -> 0 - 1\n\n\
         fn handle conn Socket -> i64\n    \
         let sent result<i64, string> = tcp_write(conn, \"ping\")\n    \
         let reply result<string, string> = tcp_read(conn)\n    \
         tcp_close(conn)\n    \
         match reply\n        \
         ok(text) -> len(text)\n        \
         err(message) -> 0 - 2\n"
    );
    let prog = std::env::temp_dir().join("lullaby_tcp_client.lby");
    std::fs::write(&prog, program).expect("write program");

    for backend in ["ast", "ir", "bytecode"] {
        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                prog.to_str().expect("program path"),
            ])
            .output()
            .expect("run cli");
        assert!(output.status.success(), "{backend}: {output:?}");
        // The reply "pong!" is 5 bytes long.
        assert_eq!(stdout(&output).trim(), "5", "{backend} reply length");
    }

    server.join().expect("server thread");
    let _ = std::fs::remove_file(&prog);
}

#[test]
pub(crate) fn tcp_server_round_trip() {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    // A real TCP round-trip where the Lullaby program is the SERVER: it listens on
    // a fixed loopback port, accepts one connection, reads the request, echoes it
    // back with a prefix, and exits. The Rust test connects as the client.
    //
    // Pick an ephemeral port up front by binding and dropping, then reuse it. This
    // is a small race window but adequate for a single-shot loopback test.
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
        probe.local_addr().expect("addr").port()
    };

    let program = format!(
        "fn main -> i64\n    \
         let bound result<Socket, string> = tcp_listen(\"127.0.0.1\", {port})\n    \
         match bound\n        \
         ok(listener) -> serve(listener)\n        \
         err(message) -> 0 - 1\n\n\
         fn serve listener Socket -> i64\n    \
         let accepted result<Socket, string> = tcp_accept(listener)\n    \
         match accepted\n        \
         ok(conn) -> echo(conn)\n        \
         err(message) -> 0 - 2\n\n\
         fn echo conn Socket -> i64\n    \
         let request result<string, string> = tcp_read(conn)\n    \
         match request\n        \
         ok(text) -> reply(conn, text)\n        \
         err(message) -> 0 - 3\n\n\
         fn reply conn Socket text string -> i64\n    \
         let sent result<i64, string> = tcp_write(conn, \"echo:\" + text)\n    \
         tcp_close(conn)\n    \
         match sent\n        \
         ok(count) -> count\n        \
         err(message) -> 0 - 4\n"
    );
    let prog = std::env::temp_dir().join("lullaby_tcp_server.lby");
    std::fs::write(&prog, program).expect("write program");

    // Run the Lullaby server in a background thread so the test can connect to it.
    let prog_path = prog.clone();
    let server = std::thread::spawn(move || {
        lullaby()
            .args([
                "run",
                "--backend",
                "ast",
                prog_path.to_str().expect("program path"),
            ])
            .output()
            .expect("run cli")
    });

    // Retry the connect briefly while the Lullaby server binds and starts listening.
    let mut stream = None;
    for _ in 0..50 {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
    let mut stream = stream.expect("connect to lullaby server");
    stream.write_all(b"hi").expect("client write");
    stream.flush().expect("client flush");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("client read");
    assert_eq!(reply, "echo:hi", "server echo reply");

    let output = server.join().expect("server thread");
    assert!(output.status.success(), "lullaby server: {output:?}");
    // "echo:hi" is 7 bytes, the byte count returned by tcp_write.
    assert_eq!(stdout(&output).trim(), "7", "server tcp_write byte count");
    let _ = std::fs::remove_file(&prog);
}

/// Probe whether UDP loopback datagrams actually flow in this environment. Some
/// sandboxes and host firewalls silently drop loopback UDP, which would make a
/// real round-trip hang or fail through no fault of the code under test. Returns
/// `true` only if a datagram sent to a bound loopback socket is received back
/// within a short timeout.
pub(crate) fn udp_loopback_available() -> bool {
    use std::net::UdpSocket;
    use std::time::Duration;

    let Ok(rx) = UdpSocket::bind("127.0.0.1:0") else {
        return false;
    };
    let Ok(addr) = rx.local_addr() else {
        return false;
    };
    if rx
        .set_read_timeout(Some(Duration::from_millis(500)))
        .is_err()
    {
        return false;
    }
    let Ok(tx) = UdpSocket::bind("127.0.0.1:0") else {
        return false;
    };
    if tx.send_to(b"probe", addr).is_err() {
        return false;
    }
    let mut buffer = [0u8; 8];
    rx.recv_from(&mut buffer).is_ok()
}

#[test]
pub(crate) fn udp_round_trip_on_all_backends() {
    use std::net::UdpSocket;
    use std::time::Duration;

    // Skip cleanly where UDP loopback is unavailable (sandbox/firewall): the
    // round-trip would otherwise hang or fail on the environment, not the code.
    if !udp_loopback_available() {
        eprintln!(
            "skipping udp_round_trip_on_all_backends: UDP loopback is unavailable in this environment"
        );
        return;
    }

    // A real UDP round-trip: the Lullaby program binds a UDP socket, sends a
    // datagram to a Rust-side UDP socket, then receives the Rust reply and returns
    // its length. A fresh Rust responder socket is used per backend so datagrams
    // never cross runs.
    let program_template = |responder_port: u16| {
        format!(
            "fn main -> i64\n    \
             let bound result<Socket, string> = udp_bind(\"127.0.0.1\", 0)\n    \
             match bound\n        \
             ok(sock) -> exchange(sock, {responder_port})\n        \
             err(message) -> 0 - 1\n\n\
             fn exchange sock Socket responder i64 -> i64\n    \
             let sent result<i64, string> = udp_send_to(sock, \"ping\", \"127.0.0.1\", responder)\n    \
             let reply result<string, string> = udp_recv(sock)\n    \
             match reply\n        \
             ok(text) -> len(text)\n        \
             err(message) -> 0 - 2\n"
        )
    };

    for backend in ["ast", "ir", "bytecode"] {
        let responder = UdpSocket::bind("127.0.0.1:0").expect("responder bind");
        let responder_port = responder.local_addr().expect("addr").port();
        // A generous read timeout means a lost datagram surfaces as a failed
        // assertion below rather than hanging the responder thread forever.
        responder
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("responder read timeout");

        let handler = std::thread::spawn(move || {
            let mut buffer = [0u8; 64];
            if let Ok((_len, sender)) = responder.recv_from(&mut buffer) {
                let _ = responder.send_to(b"pong-udp", sender);
            }
        });

        let program = program_template(responder_port);
        let prog = std::env::temp_dir().join(format!("lullaby_udp_{backend}.lby"));
        std::fs::write(&prog, program).expect("write program");

        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                prog.to_str().expect("program path"),
            ])
            .output()
            .expect("run cli");
        assert!(output.status.success(), "{backend}: {output:?}");
        // The reply "pong-udp" is 8 bytes long.
        assert_eq!(stdout(&output).trim(), "8", "{backend} udp reply length");

        handler.join().expect("responder thread");
        let _ = std::fs::remove_file(&prog);
    }
}

#[test]
pub(crate) fn http_get_round_trip_on_all_backends() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    // A real HTTP/1.1 GET round-trip driven from the test as the SERVER. The
    // minimal server replies "hello" (5 bytes) with a `Content-Length` header and
    // `Connection: close` to every request, once per backend. The Lullaby program
    // is the client: it takes the port as a program argument via `args()`, builds
    // the URL, `http_get`s it, and returns the response body length (5).
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let port = listener.local_addr().expect("addr").port();

    let server = std::thread::spawn(move || {
        for _ in 0..3 {
            let (mut stream, _addr) = listener.accept().expect("accept");
            let mut buffer = [0u8; 1024];
            let _read = stream.read(&mut buffer).expect("server read");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                )
                .expect("server write");
            stream.flush().expect("server flush");
        }
    });

    // `args()` yields `list<string>`; `get(args(), 0)` is the port passed on the
    // command line. The URL is assembled with `string` concatenation.
    let program = concat!(
        "fn main -> i64\n    ",
        "let port string = get(args(), 0)\n    ",
        "let url string = \"http://127.0.0.1:\" + port + \"/\"\n    ",
        "let outcome result<string, string> = http_get(url)\n    ",
        "match outcome\n        ",
        "ok(body) -> len(body)\n        ",
        "err(message) -> 0 - 1\n",
    );
    let prog = std::env::temp_dir().join("lullaby_http_get.lby");
    std::fs::write(&prog, program).expect("write program");
    let port_arg = port.to_string();

    for backend in ["ast", "ir", "bytecode"] {
        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                prog.to_str().expect("program path"),
                &port_arg,
            ])
            .output()
            .expect("run cli");
        assert!(output.status.success(), "{backend}: {output:?}");
        // The body "hello" is 5 bytes long.
        assert_eq!(stdout(&output).trim(), "5", "{backend} body length");
    }

    server.join().expect("server thread");
    let _ = std::fs::remove_file(&prog);
}

#[test]
pub(crate) fn http_post_round_trip_on_all_backends() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    // A real HTTP/1.1 POST round-trip: the minimal server reads the request,
    // parses `Content-Length`, drains the request body, and replies with the body
    // byte count rendered as the response body. The Lullaby program posts a fixed
    // body and returns the length of the response body (which is the decimal
    // digits of the request body length). The request body is "payload" (7 bytes),
    // so the response body is "7" (1 byte) and the Lullaby program returns 1.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let port = listener.local_addr().expect("addr").port();

    let server = std::thread::spawn(move || {
        for _ in 0..3 {
            let (mut stream, _addr) = listener.accept().expect("accept");
            let mut raw = Vec::new();
            let mut buffer = [0u8; 1024];
            // Read until the header terminator, then keep reading the declared body.
            loop {
                let read = stream.read(&mut buffer).expect("server read");
                if read == 0 {
                    break;
                }
                raw.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&raw);
                if let Some(header_end) = text.find("\r\n\r\n") {
                    let length = text
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("Content-Length:")
                                .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    let body_start = header_end + 4;
                    if raw.len() >= body_start + length {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&raw);
            let header_end = text.find("\r\n\r\n").expect("header terminator");
            let length = text
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Content-Length:")
                        .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            let body = &raw[header_end + 4..header_end + 4 + length];
            let reply_body = body.len().to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply_body.len(),
                reply_body
            );
            stream.write_all(response.as_bytes()).expect("server write");
            stream.flush().expect("server flush");
        }
    });

    let program = concat!(
        "fn main -> i64\n    ",
        "let port string = get(args(), 0)\n    ",
        "let url string = \"http://127.0.0.1:\" + port + \"/\"\n    ",
        "let outcome result<string, string> = http_post(url, \"payload\")\n    ",
        "match outcome\n        ",
        "ok(body) -> len(body)\n        ",
        "err(message) -> 0 - 1\n",
    );
    let prog = std::env::temp_dir().join("lullaby_http_post.lby");
    std::fs::write(&prog, program).expect("write program");
    let port_arg = port.to_string();

    for backend in ["ast", "ir", "bytecode"] {
        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                prog.to_str().expect("program path"),
                &port_arg,
            ])
            .output()
            .expect("run cli");
        assert!(output.status.success(), "{backend}: {output:?}");
        // The request body "payload" is 7 bytes, so the response body is "7"
        // (1 byte) and the Lullaby program returns 1.
        assert_eq!(stdout(&output).trim(), "1", "{backend} echoed length");
    }

    server.join().expect("server thread");
    let _ = std::fs::remove_file(&prog);
}

/// End-to-end HTTP/1.1 round-trip where the Lullaby program is the SERVER,
/// written in pure Lullaby (`examples/valid/http_server/server.lby`) on top of
/// the socket builtins plus `tcp_shutdown`. A Rust `TcpStream` HTTP client
/// sends a real request and reads the full response to EOF, asserting the
/// status line and body â€” proving a graceful teardown delivers the buffered
/// response (no "Empty reply"). Runs on every backend.
#[test]
pub(crate) fn http_server_round_trip_on_all_backends() {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    // Send one HTTP request over a fresh connection and read the whole response
    // to EOF (the server sends `Connection: close` and shuts down its write half).
    fn request(port: u16, path: &str) -> String {
        // Retry the connect briefly while the Lullaby server binds and listens.
        let mut stream = None;
        for _ in 0..100 {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(connected) => {
                    stream = Some(connected);
                    break;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
        let mut stream = stream.expect("connect to lullaby http server");
        let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).expect("client write");
        stream.flush().expect("client flush");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("client read to EOF");
        response
    }

    let server_path = workspace_root().join("examples/valid/http_server/server.lby");

    for backend in ["ast", "ir", "bytecode"] {
        // Pick a free port, then release it so the Lullaby server can bind it.
        let port = {
            let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
            probe.local_addr().expect("addr").port()
        };

        // Serve two requests: one for `/` and one for an unknown path.
        let path = server_path.clone();
        let port_arg = port.to_string();
        let server = std::thread::spawn(move || {
            lullaby()
                .args([
                    "run",
                    "--backend",
                    backend,
                    path.to_str().expect("server path"),
                    &port_arg,
                    "2",
                ])
                .output()
                .expect("run cli")
        });

        // Known route: expect a 200 with the server's greeting body.
        let ok_response = request(port, "/");
        let status_line = ok_response.lines().next().unwrap_or_default();
        assert_eq!(
            status_line, "HTTP/1.1 200 OK",
            "{backend} status line for /: {ok_response:?}"
        );
        assert!(
            ok_response.ends_with("Hello from Lullaby!"),
            "{backend} greeting body for /: {ok_response:?}"
        );
        assert!(
            ok_response.contains("Content-Length: 19"),
            "{backend} content-length for /: {ok_response:?}"
        );

        // Unknown route: expect a 404.
        let missing_response = request(port, "/does-not-exist");
        let missing_status = missing_response.lines().next().unwrap_or_default();
        assert_eq!(
            missing_status, "HTTP/1.1 404 Not Found",
            "{backend} status line for unknown path: {missing_response:?}"
        );

        let output = server.join().expect("server thread");
        assert!(
            output.status.success(),
            "{backend} lullaby server: {output:?}"
        );
    }
}

// -- WebAssembly backend (scalar subset) -------------------------------------

/// Whether `node` is available on this machine (its result runs the emitted
/// `.wasm` for execution parity). Returns `false` if `node --version` cannot run.
pub(crate) fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[path = "cli/suite2.rs"]
mod suite2;

// -- Native x86-64 backend (i64-scalar subset, link-to-exe) ------------------

/// Locate `rust-lld.exe` under the rustc sysroot (mirrors the CLI's discovery).
/// `None` if rustc or the linker cannot be found.
pub(crate) fn rust_lld_path() -> Option<PathBuf> {
    let out = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let lld = PathBuf::from(sysroot).join("lib/rustlib/x86_64-pc-windows-msvc/bin/rust-lld.exe");
    lld.is_file().then_some(lld)
}

/// Locate a toolchain executable on `PATH` (e.g. `llvm-pdbutil`) for optional,
/// gracefully-skipped real-toolchain checks. Tries the bare name and `.exe`.
pub(crate) fn find_tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for candidate in [dir.join(name), dir.join(format!("{name}.exe"))] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Whether `kernel32.lib` is reachable via the `LIB` environment variable.
pub(crate) fn kernel32_available() -> bool {
    std::env::var("LIB").ok().is_some_and(|lib| {
        lib.split(';')
            .any(|dir| !dir.is_empty() && PathBuf::from(dir.trim()).join("kernel32.lib").is_file())
    })
}

/// Emit + verbose-list the native object for the i64-scalar fixture. This part
/// always runs: it exercises the emitter and CLI wiring regardless of linking.
#[test]
pub(crate) fn native_emits_object_and_lists_functions() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_native_list.exe");
    let output = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{}", stderr(&output));
    let listing = stdout(&output);
    for name in ["add", "fib", "sum_to", "main"] {
        assert!(
            listing.contains(&format!("compiled {name}")),
            "expected `{name}` compiled: {listing}"
        );
    }

    // The object file is always written (the reliable floor) and starts with the
    // AMD64 COFF machine magic (0x8664, little-endian).
    let obj = out.with_extension("obj");
    let bytes = std::fs::read(&obj).expect("read native object");
    assert_eq!(&bytes[0..2], &[0x64, 0x86], "COFF AMD64 machine");
}

/// `lullaby native --target x86_64-unknown-linux-gnu` writes a relocatable ELF64
/// object beginning with the ELF magic. On this Windows host the object is not
/// linked or run (deferred to the native platform / Phase 9 CI); the CLI reports
/// exactly that.
#[test]
pub(crate) fn native_target_linux_emits_elf_object() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_native_linux.o");
    let output = lullaby()
        .args([
            "native",
            "--target",
            "x86_64-unknown-linux-gnu",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{}", stderr(&output));
    let listing = stdout(&output);
    assert!(
        listing.contains("x86_64-unknown-linux-gnu (ELF64)"),
        "reports the ELF target: {listing}"
    );
    assert!(
        listing.contains("Phase 9") || listing.contains("deferred"),
        "reports link+run deferral: {listing}"
    );
    assert!(
        !listing.contains("native exe:"),
        "must not link an exe on this host: {listing}"
    );

    let bytes = std::fs::read(&out).expect("read ELF object");
    assert_eq!(&bytes[0..4], &[0x7f, b'E', b'L', b'F'], "ELF magic");
    assert_eq!(bytes[4], 2, "ELFCLASS64");
}

/// `lullaby native --target x86_64-apple-darwin` writes a relocatable Mach-O
/// x86-64 object beginning with the Mach-O magic, also without linking.
#[test]
pub(crate) fn native_target_macos_emits_macho_object() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_native_macos.o");
    let output = lullaby()
        .args([
            "native",
            "--target",
            "x86_64-apple-darwin",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{}", stderr(&output));

    let bytes = std::fs::read(&out).expect("read Mach-O object");
    // MH_MAGIC_64 (0xFEEDFACF) little-endian.
    assert_eq!(&bytes[0..4], &[0xCF, 0xFA, 0xED, 0xFE], "Mach-O magic");
}

/// `lullaby native --target aarch64-unknown-linux-gnu` writes a real aarch64
/// ELF64 object: the ELF magic, `EM_AARCH64` (183), the compiled scalar
/// functions, and the aarch64-specific link/run notice (not the x86-64 "deferred"
/// notice). This structural part always runs.
#[test]
pub(crate) fn native_target_aarch64_emits_elf_object() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_native_aarch64.o");
    let output = lullaby()
        .args([
            "native",
            "--verbose",
            "--target",
            "aarch64-unknown-linux-gnu",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{}", stderr(&output));
    let listing = stdout(&output);
    assert!(
        listing.contains("aarch64-unknown-linux-gnu (ELF64)"),
        "reports the aarch64 ELF target: {listing}"
    );
    assert!(
        listing.contains("aarch64 ELF object emitted"),
        "reports the aarch64 link/run notice: {listing}"
    );
    for name in ["add", "fib", "sum_to", "main"] {
        assert!(
            listing.contains(&format!("compiled {name}")),
            "expected `{name}` compiled: {listing}"
        );
    }

    let bytes = std::fs::read(&out).expect("read aarch64 ELF object");
    assert_eq!(&bytes[0..4], &[0x7f, b'E', b'L', b'F'], "ELF magic");
    assert_eq!(bytes[4], 2, "ELFCLASS64");
    assert_eq!(
        u16::from_le_bytes([bytes[18], bytes[19]]),
        183,
        "e_machine = EM_AARCH64"
    );
}

/// Locate the LLVM cross-linker `ld.lld` shipped with the Rust toolchain — this
/// is `rust-lld` in gnu (ELF) flavor. `None` if it cannot be found.
pub(crate) fn ld_lld_path() -> Option<PathBuf> {
    let out = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let lld =
        PathBuf::from(sysroot).join("lib/rustlib/x86_64-pc-windows-msvc/bin/gcc-ld/ld.lld.exe");
    lld.is_file().then_some(lld)
}

/// Whether Docker with working arm64 (QEMU) emulation is available: probe with a
/// throwaway `linux/arm64` container, exactly as the task describes.
pub(crate) fn docker_arm64_available() -> bool {
    Command::new("docker")
        .args(["run", "--rm", "--platform", "linux/arm64", "alpine", "true"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// End-to-end AArch64 verification: emit the aarch64 ELF, link it with the
/// cross-linker (`ld.lld -m aarch64linux`) into an arm64 executable, run it under
/// Docker's arm64 (QEMU) emulation, and assert the process exit code equals the
/// interpreter's `run` result mod 256. Gated on Docker+arm64 and `ld.lld` being
/// available; skipped gracefully otherwise (like the node-gated WASM parity
/// tests). This is the real link+run proof that the AArch64 machine code is
/// correct, not just structurally well-formed.
#[test]
pub(crate) fn native_aarch64_links_and_runs_under_docker() {
    let Some(lld) = ld_lld_path() else {
        eprintln!("ld.lld not found in the Rust sysroot; skipping AArch64 link+run");
        return;
    };
    if !docker_arm64_available() {
        eprintln!("Docker arm64 emulation unavailable; skipping AArch64 link+run");
        return;
    }
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");

    // The expected exit code is the interpreter's `run` result mod 256.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run interpreter");
    assert!(run.status.success(), "{}", stderr(&run));
    let result: i64 = stdout(&run)
        .lines()
        .filter_map(|line| line.trim().parse::<i64>().ok())
        .next_back()
        .expect("interpreter prints an integer result");
    let expected_code = result.rem_euclid(256) as i32;

    // Fresh working directory for the object + linked executable.
    let dir = std::env::temp_dir().join("lullaby_aarch64_run");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create work dir");
    let obj = dir.join("prog.o");
    let exe = dir.join("prog");

    // 1. Emit the aarch64 ELF object.
    let emit = lullaby()
        .args([
            "native",
            "--target",
            "aarch64-unknown-linux-gnu",
            "-o",
            obj.to_str().expect("obj path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("emit aarch64 object");
    assert!(emit.status.success(), "{}", stderr(&emit));

    // 2. Link it into an arm64 executable with the cross-linker.
    let link = Command::new(&lld)
        .args([
            "-m",
            "aarch64linux",
            "-o",
            exe.to_str().expect("exe path"),
            obj.to_str().expect("obj path"),
        ])
        .output()
        .expect("run ld.lld");
    assert!(
        link.status.success(),
        "ld.lld failed: {}",
        String::from_utf8_lossy(&link.stderr)
    );

    // 3. Run under arm64 emulation. Windows bind mounts do not carry the exec
    //    bit, so copy the binary and mark it executable before running it.
    let mount = format!("{}:/w", dir.display());
    let run_exe = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--platform",
            "linux/arm64",
            "-v",
            &mount,
            "busybox",
            "sh",
            "-c",
            "cp /w/prog /prog && chmod +x /prog && /prog",
        ])
        .output()
        .expect("docker run arm64");

    // 4. The container exit code must equal the interpreter result mod 256.
    let code = run_exe.status.code().expect("container exit code");
    assert_eq!(
        code,
        expected_code,
        "aarch64 exit {code} must equal interpreter result {result} mod 256 ({expected_code}); docker stderr: {}",
        String::from_utf8_lossy(&run_exe.stderr)
    );
}

/// End-to-end AArch64 verification of the `i64` scalar-math builtins
/// (`abs`/`min`/`max`/`gcd`/`sign`/`clamp`): compile the builtin fixture to an
/// aarch64 ELF, link it with `ld.lld -m aarch64linux`, run it under Docker's
/// arm64 (QEMU) emulation, and assert the process exit code equals the
/// interpreter's `run` result mod 256 (162). Every builtin is exercised, both
/// inline in `main` and across a function boundary (`clip`/`reduce` helpers,
/// including a `lo > hi` clamp). Gated on Docker+arm64 and `ld.lld`; skipped
/// gracefully otherwise — the exact-byte unit tests in `aarch64.rs` pin the
/// emitted encodings regardless.
#[test]
pub(crate) fn native_aarch64_math_builtins_link_and_run_parity() {
    let Some(lld) = ld_lld_path() else {
        eprintln!("ld.lld not found in the Rust sysroot; skipping AArch64 builtin link+run");
        return;
    };
    if !docker_arm64_available() {
        eprintln!("Docker arm64 emulation unavailable; skipping AArch64 builtin link+run");
        return;
    }
    let fixture = workspace_root().join("tests/fixtures/valid/aarch64_math_builtins.lby");

    // The expected exit code is the interpreter's `run` result mod 256.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run interpreter");
    assert!(run.status.success(), "{}", stderr(&run));
    let result: i64 = stdout(&run)
        .lines()
        .filter_map(|line| line.trim().parse::<i64>().ok())
        .next_back()
        .expect("interpreter prints an integer result");
    assert_eq!(result, 162, "builtin fixture computes 162");
    let expected_code = result.rem_euclid(256) as i32;

    let dir = std::env::temp_dir().join("lullaby_aarch64_builtins_run");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create work dir");
    let obj = dir.join("prog.o");
    let exe = dir.join("prog");

    // 1. Emit the aarch64 ELF object; every function must compile (not skip).
    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "--target",
            "aarch64-unknown-linux-gnu",
            "-o",
            obj.to_str().expect("obj path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("emit aarch64 object");
    assert!(emit.status.success(), "{}", stderr(&emit));
    let listing = stdout(&emit);
    for name in ["reduce", "clip", "main"] {
        assert!(
            listing.contains(&format!("compiled {name}")),
            "expected `{name}` compiled natively on AArch64: {listing}"
        );
    }

    // 2. Link it into an arm64 executable with the cross-linker.
    let link = Command::new(&lld)
        .args([
            "-m",
            "aarch64linux",
            "-o",
            exe.to_str().expect("exe path"),
            obj.to_str().expect("obj path"),
        ])
        .output()
        .expect("run ld.lld");
    assert!(
        link.status.success(),
        "ld.lld failed: {}",
        String::from_utf8_lossy(&link.stderr)
    );

    // 3. Run under arm64 emulation (Windows bind mounts drop the exec bit).
    let mount = format!("{}:/w", dir.display());
    let run_exe = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--platform",
            "linux/arm64",
            "-v",
            &mount,
            "busybox",
            "sh",
            "-c",
            "cp /w/prog /prog && chmod +x /prog && /prog",
        ])
        .output()
        .expect("docker run arm64");

    // 4. The container exit code must equal the interpreter result mod 256.
    let code = run_exe.status.code().expect("container exit code");
    assert_eq!(
        code,
        expected_code,
        "aarch64 builtin exit {code} must equal interpreter result {result} mod 256 ({expected_code}); docker stderr: {}",
        String::from_utf8_lossy(&run_exe.stderr)
    );
}

/// Whether Docker can run a native `linux/amd64` container (no QEMU needed).
pub(crate) fn docker_amd64_available() -> bool {
    Command::new("docker")
        .args(["run", "--rm", "--platform", "linux/amd64", "alpine", "true"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// End-to-end x86-64 ELF verification: emit the Linux x86-64 ELF, link it with
/// `ld.lld -m elf_x86_64`, run it under a native `linux/amd64` Docker container,
/// and assert the process exit code equals the interpreter's `run` result mod
/// 256. This proves the x86-64 ELF machine code + freestanding `exit`-syscall
/// entry actually execute on Linux, not merely that the object is well-formed.
/// Gated on Docker + `ld.lld`; skipped gracefully otherwise.
#[test]
pub(crate) fn native_elf_x86_64_links_and_runs_under_docker() {
    let Some(lld) = ld_lld_path() else {
        eprintln!("ld.lld not found in the Rust sysroot; skipping x86-64 ELF link+run");
        return;
    };
    if !docker_amd64_available() {
        eprintln!("Docker linux/amd64 unavailable; skipping x86-64 ELF link+run");
        return;
    }
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run interpreter");
    assert!(run.status.success(), "{}", stderr(&run));
    let result: i64 = stdout(&run)
        .lines()
        .filter_map(|line| line.trim().parse::<i64>().ok())
        .next_back()
        .expect("interpreter prints an integer result");
    let expected_code = result.rem_euclid(256) as i32;

    let dir = std::env::temp_dir().join("lullaby_elf_x86_64_run");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create work dir");
    let obj = dir.join("prog.o");
    let exe = dir.join("prog");
    let emit = lullaby()
        .args([
            "native",
            "--target",
            "x86_64-unknown-linux-gnu",
            "-o",
            obj.to_str().expect("obj path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("emit x86-64 ELF object");
    assert!(emit.status.success(), "{}", stderr(&emit));
    let link = Command::new(&lld)
        .args([
            "-m",
            "elf_x86_64",
            "-o",
            exe.to_str().expect("exe path"),
            obj.to_str().expect("obj path"),
        ])
        .output()
        .expect("run ld.lld");
    assert!(
        link.status.success(),
        "ld.lld failed: {}",
        String::from_utf8_lossy(&link.stderr)
    );
    let mount = format!("{}:/w", dir.display());
    let run_exe = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--platform",
            "linux/amd64",
            "-v",
            &mount,
            "busybox",
            "sh",
            "-c",
            "cp /w/prog /prog && chmod +x /prog && /prog",
        ])
        .output()
        .expect("docker run amd64");
    let code = run_exe.status.code().expect("container exit code");
    assert_eq!(
        code,
        expected_code,
        "x86-64 ELF exit {code} must equal interpreter result {result} mod 256 ({expected_code}); docker stderr: {}",
        String::from_utf8_lossy(&run_exe.stderr)
    );
}

/// An unknown `--target` triple is rejected with `L0347` and no object is
/// produced.
#[test]
pub(crate) fn native_unknown_target_is_rejected() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let output = lullaby()
        .args([
            "native",
            "--target",
            "riscv64-unknown-linux-gnu",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "unknown target must fail");
    let combined = format!("{}{}", stdout(&output), stderr(&output));
    assert!(combined.contains("L0347"), "reports L0347: {combined}");
}

/// `lullaby native --debug` must emit a CodeView `.debug$S` source-line section
/// (opt-in) and print the debug notice, while the default (no `--debug`) object
/// stays byte-for-byte identical. This structural part always runs. If
/// `llvm-pdbutil` is discoverable it optionally reads back the CodeView stream.
#[test]
pub(crate) fn native_debug_emits_codeview_line_info() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_native_debug.exe");

    let output = lullaby()
        .args([
            "native",
            "--debug",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("debug info: CodeView"),
        "expected the debug notice: {}",
        stdout(&output)
    );

    // The debug object carries a `.debug$S` section (searched in the section
    // header table: NumberOfSections at header offset 2, 40-byte headers after
    // the 20-byte COFF header, 8-byte name field).
    let obj = out.with_extension("obj");
    let bytes = std::fs::read(&obj).expect("read native debug object");
    let num_sections = u16::from_le_bytes([bytes[2], bytes[3]]) as usize;
    let mut debug_hdr = None;
    for i in 0..num_sections {
        let hdr = 20 + i * 40;
        if &bytes[hdr..hdr + 8] == b".debug\x24S" {
            debug_hdr = Some(hdr);
        }
    }
    let hdr = debug_hdr.expect("`.debug$S` section present with --debug");

    // Its raw data begins with the CodeView C13 signature (4), and the source
    // file name and per-function declaration line (`main` on line 15) are
    // recoverable from the stream bytes.
    let raw_ptr = u32::from_le_bytes(bytes[hdr + 20..hdr + 24].try_into().unwrap()) as usize;
    let raw_size = u32::from_le_bytes(bytes[hdr + 16..hdr + 20].try_into().unwrap()) as usize;
    let section = &bytes[raw_ptr..raw_ptr + raw_size];
    assert_eq!(
        u32::from_le_bytes(section[0..4].try_into().unwrap()),
        4,
        "CodeView C13 signature"
    );
    assert!(
        section
            .windows(b"native_scalars.lby".len())
            .any(|w| w == b"native_scalars.lby"),
        "source file name recorded in the debug section"
    );

    // Without `--debug`, the object has no `.debug$S` section and is byte-for-byte
    // the default native object.
    let plain_out = std::env::temp_dir().join("lullaby_native_debug_off.exe");
    let plain = lullaby()
        .args([
            "native",
            "-o",
            plain_out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(plain.status.success(), "{}", stderr(&plain));
    let plain_bytes =
        std::fs::read(plain_out.with_extension("obj")).expect("read plain native object");
    let plain_sections = u16::from_le_bytes([plain_bytes[2], plain_bytes[3]]) as usize;
    for i in 0..plain_sections {
        let ph = 20 + i * 40;
        assert_ne!(
            &plain_bytes[ph..ph + 8],
            b".debug\x24S",
            "default object must have no debug section"
        );
    }

    // Optional real-toolchain readback. Prefer `llvm-readobj` (bundled with the
    // rustc toolchain that already provides `rust-lld`), else any `llvm-pdbutil`
    // or `llvm-readobj` on PATH. When found, decode the CodeView stream and assert
    // it surfaces the source file plus the `main` declaration line (15). Skip
    // gracefully when no such tool is discoverable.
    let readobj = llvm_readobj_path().or_else(|| find_tool("llvm-readobj"));
    if let Some(tool) = readobj {
        let dump = Command::new(tool)
            .args(["--codeview", obj.to_str().expect("obj path")])
            .output();
        if let Ok(dump) = dump {
            if dump.status.success() {
                let text = String::from_utf8_lossy(&dump.stdout);
                assert!(
                    text.contains("native_scalars.lby"),
                    "llvm-readobj should surface the source file: {text}"
                );
                assert!(
                    text.contains("LineNumberStart: 15"),
                    "llvm-readobj should surface `main`'s declaration line 15: {text}"
                );
            } else {
                eprintln!("llvm-readobj --codeview failed; skipping readback assertion");
            }
        }
    } else {
        eprintln!("no llvm-readobj/llvm-pdbutil found; skipping CodeView readback");
    }
}

/// Locate `llvm-readobj.exe` in the rustc toolchain bin dir (alongside
/// `rust-lld`). `None` if the toolchain or tool cannot be found.
pub(crate) fn llvm_readobj_path() -> Option<PathBuf> {
    let out = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let tool =
        PathBuf::from(sysroot).join("lib/rustlib/x86_64-pc-windows-msvc/bin/llvm-readobj.exe");
    tool.is_file().then_some(tool)
}

/// A file with no i64-scalar function eligible reports diagnostic `L0339`.
#[path = "cli/suite3.rs"]
mod suite3;

// Unique-per-process self-cleaning temp dirs; see `cli/scratch.rs`. Required for
// any test that writes and then RUNS an `.exe`.
#[path = "cli/scratch.rs"]
mod scratch;
pub(crate) use scratch::ScratchDir;

#[path = "cli/fuzz.rs"]
mod fuzz;

// -- Standard-input builtins (`read_line` / `read_all`) ----------------------
#[path = "cli/suite4.rs"]
mod suite4;

// -- Map iteration builtins (`map_keys` / `map_values`) ----------------------
#[path = "cli/suite5.rs"]
mod suite5;

// -- `match` as an expression, with block arm bodies -------------------------
#[path = "cli/suite6.rs"]
mod suite6;

// -- Named compile-time constants (`const NAME type = <expr>`) ----------------
#[path = "cli/suite7.rs"]
mod suite7;

// -- User-defined generic structs, stage 1 (`struct Box<T>`) ------------------
#[path = "cli/suite8.rs"]
mod suite8;

// -- User-defined generic enums, stage A1 (`enum Opt<T>`) --------------------
#[path = "cli/suite9.rs"]
mod suite9;

// -- Methods on generic types, stage 4 (`impl Box<T>`) -----------------------
#[path = "cli/suite10.rs"]
mod suite10;

// -- Multi-parameter + bounded generic types, stage 5 ------------------------
#[path = "cli/suite11.rs"]
mod suite11;

// -- Explicit i64 overflow arithmetic + checked_div/checked_rem --------------
#[path = "cli/suite12.rs"]
mod suite12;

// -- Safe-tier failure semantics (A5): abort vs recoverable, backend-consistent
#[path = "cli/suite13.rs"]
mod suite13;

// -- Actor concurrency model, stage 1: `actor`/`state`/`init`/`on`, `spawn`, `tell`
#[path = "cli/suite14.rs"]
mod suite14;

// -- FFI callbacks (A3): passing a Lullaby function to C as a C-ABI function pointer
#[path = "cli/ffi_callbacks.rs"]
mod ffi_callbacks;

// -- Freestanding / kernel tier, stage 1: the `no-runtime` directive + tier gate
#[path = "cli/suite15.rs"]
mod suite15;

// -- Value-position branch/arm tail parity (the branch-local aggregate miscompile)
#[path = "cli/suite16.rs"]
mod suite16;

// -- The built-in test runner (`lullaby test`): discovery, failure isolation,
//    deterministic ordering, and `--filter` (road_to_1_0_stable B3)
#[path = "cli/suite17.rs"]
mod suite17;

// -- Interim heap-box builtins (`alloc`/`dealloc`) on the native backend
#[path = "cli/suite18.rs"]
mod suite18;

#[path = "cli/suite19.rs"]
mod suite19;

// -- DWARF source-line debug info on the ELF/Mach-O targets (`--debug`/`-g`),
//    the portable counterpart of the COFF CodeView path (road_to_1_0_stable B3)
#[path = "cli/suite20.rs"]
mod suite20;

// -- Three frontend semantics fixes: model-preserving `ptr_cast`, void `export fn`,
//    and `L0350` over direct copy aliases
#[path = "cli/suite21.rs"]
mod suite21;

// -- Packed narrow array elements: walking a narrow-element buffer (`array<i32>`,
//    `array<u8>`, ...) through raw pointers (road_to_1_0_stable C3)
#[path = "cli/suite22.rs"]
mod suite22;

/// Whether `ucrt.lib` (the C runtime import library, providing `llabs`) is
/// reachable via the `LIB` environment variable, like `kernel32_available`.
pub(crate) fn ucrt_available() -> bool {
    std::env::var("LIB").ok().is_some_and(|lib| {
        lib.split(';')
            .any(|dir| !dir.is_empty() && PathBuf::from(dir.trim()).join("ucrt.lib").is_file())
    })
}

/// Best-effort: if the MSVC `LIB` environment variable is not already set (so a
/// native link would skip), construct it from the installed MSVC toolset and
/// Windows SDK x64 library directories and set it in this test process's
/// environment. The child `lullaby native` invocation inherits the environment,
/// so it can then discover `kernel32.lib`/`ucrt.lib` and actually link + run. A
/// no-op when `LIB` is already set or no MSVC/SDK install is found — the link+run
/// step then skips gracefully as before. Windows-only.
///
/// The directories are located directly on the filesystem (rather than by sourcing
/// `vcvars64.bat`) so the setup is independent of any shell quoting: `LIB` is the
/// concatenation of the MSVC toolset `lib\x64`, the Windows SDK `ucrt\x64`, and the
/// Windows SDK `um\x64` directories — exactly the three vcvars adds for a link.
pub(crate) fn ensure_msvc_env() {
    if std::env::var_os("LIB").is_some() {
        return;
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(msvc_lib) = latest_msvc_lib_x64() {
        dirs.push(msvc_lib);
    }
    let (ucrt, um) = latest_sdk_lib_x64();
    dirs.extend(ucrt);
    dirs.extend(um);
    // Only set `LIB` if we actually found the two the linker needs (kernel32.lib
    // lives in `um\x64`, ucrt.lib in `ucrt\x64`); otherwise leave it unset so the
    // gated tests skip cleanly.
    let has_kernel32 = dirs.iter().any(|d| d.join("kernel32.lib").is_file());
    let has_ucrt = dirs.iter().any(|d| d.join("ucrt.lib").is_file());
    if !has_kernel32 || !has_ucrt {
        return;
    }
    let joined = dirs
        .iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(";");
    // SAFETY: called from single-threaded test setup, before spawning any child.
    unsafe { std::env::set_var("LIB", joined) };
}

/// The newest MSVC toolset `lib\x64` directory across the known VS 2022 install
/// roots (Enterprise/Professional/Community/BuildTools), or `None` if none exist.
pub(crate) fn latest_msvc_lib_x64() -> Option<PathBuf> {
    let mut best: Option<(String, PathBuf)> = None;
    for base in vs_2022_roots() {
        let tools = base.join("VC\\Tools\\MSVC");
        let Ok(entries) = std::fs::read_dir(&tools) else {
            continue;
        };
        for entry in entries.flatten() {
            let lib = entry.path().join("lib\\x64");
            if lib.join("libcmt.lib").is_file() || lib.is_dir() {
                let version = entry.file_name().to_string_lossy().into_owned();
                if best.as_ref().is_none_or(|(v, _)| version > *v) {
                    best = Some((version, lib));
                }
            }
        }
    }
    best.map(|(_, path)| path)
}

/// The newest Windows SDK `ucrt\x64` and `um\x64` library directories (each as a
/// single-element vec, or empty if absent).
pub(crate) fn latest_sdk_lib_x64() -> (Vec<PathBuf>, Vec<PathBuf>) {
    for program_files in [
        std::env::var_os("ProgramFiles(x86)"),
        std::env::var_os("ProgramFiles"),
    ]
    .into_iter()
    .flatten()
    {
        let lib_root = PathBuf::from(&program_files).join("Windows Kits\\10\\Lib");
        let Ok(entries) = std::fs::read_dir(&lib_root) else {
            continue;
        };
        let mut best: Option<(String, PathBuf)> = None;
        for entry in entries.flatten() {
            let version = entry.file_name().to_string_lossy().into_owned();
            if entry.path().join("um\\x64\\kernel32.lib").is_file()
                && best.as_ref().is_none_or(|(v, _)| version > *v)
            {
                best = Some((version, entry.path()));
            }
        }
        if let Some((_, sdk)) = best {
            return (vec![sdk.join("ucrt\\x64")], vec![sdk.join("um\\x64")]);
        }
    }
    (Vec::new(), Vec::new())
}

/// The known Visual Studio 2022 install roots (per edition) on this machine.
pub(crate) fn vs_2022_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for base in [
        "C:\\Program Files\\Microsoft Visual Studio\\2022",
        "C:\\Program Files (x86)\\Microsoft Visual Studio\\2022",
    ] {
        for edition in ["Enterprise", "Professional", "Community", "BuildTools"] {
            let root = PathBuf::from(base).join(edition);
            if root.is_dir() {
                roots.push(root);
            }
        }
    }
    roots
}

/// C-ABI FFI: a program that declares `extern fn llabs x i64 -> i64` and returns
/// `llabs(-7)`. On the interpreters the extern call is rejected with `L0423`
/// (they cannot execute C). Native-compiled and linked against the C runtime
/// (`ucrt.lib`), the `.exe` calls the real C `llabs` and exits with code 7.
/// Gated on `rust-lld` + `kernel32.lib` + `ucrt.lib`; skips gracefully otherwise.
#[test]
pub(crate) fn ffi_calls_c_abs_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/native_only/ffi_llabs.lby");

    // `check` validates the extern declaration and its call site.
    let check = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(check.status.success(), "{}", stderr(&check));

    // Every interpreter backend rejects the extern call with L0423 rather than
    // panicking or silently no-op-ing.
    for backend in ["ast", "ir", "bytecode"] {
        let run = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(
            !run.status.success(),
            "extern call must fail on the {backend} interpreter"
        );
        let rendered = format!("{}{}", stdout(&run), stderr(&run));
        assert!(
            rendered.contains("L0423"),
            "expected L0423 on {backend}: {rendered}"
        );
    }

    // Native codegen: emit + link + run.
    let out = std::env::temp_dir().join("lullaby_ffi_llabs.exe");
    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    assert!(
        stdout(&emit).contains("compiled main"),
        "expected `main` compiled: {}",
        stdout(&emit)
    );

    if rust_lld_path().is_none() || !kernel32_available() || !ucrt_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib/ucrt.lib (via the LIB env var) not available; \
             skipping C-ABI FFI link+run"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(exit, 7, "llabs(-7) via C FFI must exit 7");
}

/// C-ABI FFI (non-`i64` scalar width): a program that declares
/// `extern fn toupper c i32 -> i32` and returns `to_i64(toupper(to_i32(97)))`.
/// `toupper('a')` is `'A'` (65), so the `.exe` exits with code 65. This exercises
/// the extended scalar marshalling: an `i32` C argument passed in the low bits of
/// `rcx` and an `i32` C return re-normalized in `rax` (`movsxd rax, eax`). On the
/// interpreters the extern call is rejected with `L0423`. Native-compiled and
/// linked against `ucrt.lib` (which provides `toupper`), the `.exe` calls the real
/// C `toupper`. Gated on `rust-lld` + `kernel32.lib` + `ucrt.lib`; skips otherwise.
#[test]
pub(crate) fn ffi_calls_c_toupper_i32_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/native_only/ffi_toupper.lby");

    // `check` validates the extern declaration and its call site.
    let check = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(check.status.success(), "{}", stderr(&check));

    // Every interpreter backend rejects the extern call with L0423.
    for backend in ["ast", "ir", "bytecode"] {
        let run = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(
            !run.status.success(),
            "extern call must fail on the {backend} interpreter"
        );
        let rendered = format!("{}{}", stdout(&run), stderr(&run));
        assert!(
            rendered.contains("L0423"),
            "expected L0423 on {backend}: {rendered}"
        );
    }

    // Native codegen: emit + link + run.
    let out = std::env::temp_dir().join("lullaby_ffi_toupper.exe");
    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    assert!(
        stdout(&emit).contains("compiled main"),
        "expected `main` compiled: {}",
        stdout(&emit)
    );

    if rust_lld_path().is_none() || !kernel32_available() || !ucrt_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib/ucrt.lib (via the LIB env var) not available; \
             skipping i32 C-ABI FFI link+run"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(exit, 65, "toupper('a') via C FFI must exit 65 ('A')");
}

/// C-ABI FFI (float scalar): a program that declares `extern fn sqrt x f64 -> f64`
/// and computes `sqrt(16.0)` (== 4.0), then derives a deterministic `i64` via two
/// float comparisons (`> 3.9` gives 3, `< 4.1` adds 4) so the `.exe` exits 7. This
/// exercises the Win64 float marshalling: the `f64` argument is passed in `xmm0`
/// and the `f64` return is read from `xmm0`. On the interpreters the extern call is
/// rejected with `L0423`. Native-compiled and linked against `ucrt.lib` (which
/// provides `sqrt`), the `.exe` calls the real C `sqrt`. Gated on `rust-lld` +
/// `kernel32.lib` + `ucrt.lib`; skips gracefully otherwise.
#[test]
pub(crate) fn ffi_calls_c_sqrt_f64_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/native_only/ffi_sqrt.lby");

    // `check` validates the extern declaration and its call site.
    let check = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(check.status.success(), "{}", stderr(&check));

    // Every interpreter backend rejects the extern call with L0423.
    for backend in ["ast", "ir", "bytecode"] {
        let run = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(
            !run.status.success(),
            "extern call must fail on the {backend} interpreter"
        );
        let rendered = format!("{}{}", stdout(&run), stderr(&run));
        assert!(
            rendered.contains("L0423"),
            "expected L0423 on {backend}: {rendered}"
        );
    }

    // Make MSVC's `LIB` available (source vcvars64 if it is not already set) so the
    // link+run step actually executes rather than skipping.
    ensure_msvc_env();

    // Native codegen: emit + link + run.
    let out = std::env::temp_dir().join("lullaby_ffi_sqrt.exe");
    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    assert!(
        stdout(&emit).contains("compiled main"),
        "expected `main` compiled: {}",
        stdout(&emit)
    );

    if rust_lld_path().is_none() || !kernel32_available() || !ucrt_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib/ucrt.lib (via the LIB env var) not available; \
             skipping f64 C-ABI FFI link+run"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(exit, 7, "sqrt(16.0)==4.0 via C FFI must exit 7");
}

/// C-ABI FFI (mixed float + int scalars): a program that declares
/// `extern fn ldexp x f64 e i32 -> f64` and computes `ldexp(1.5, 3)` (== 12.0),
/// then derives a deterministic `i64` via two float comparisons so the `.exe`
/// exits 12. This exercises Win64 positional register routing: the `f64` at
/// position 0 goes to `xmm0`, the `i32` at position 1 goes to integer register 1
/// (`rdx`), and the `f64` return comes back in `xmm0` — each position consuming its
/// slot in exactly one register sequence. On the interpreters the extern call is
/// rejected with `L0423`. Native-compiled and linked against `ucrt.lib` (which
/// provides `ldexp`), the `.exe` calls the real C `ldexp`. Gated on `rust-lld` +
/// `kernel32.lib` + `ucrt.lib`; skips gracefully otherwise.
#[test]
pub(crate) fn ffi_calls_c_ldexp_mixed_scalars_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/native_only/ffi_ldexp.lby");

    // `check` validates the extern declaration and its call site.
    let check = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(check.status.success(), "{}", stderr(&check));

    // Every interpreter backend rejects the extern call with L0423.
    for backend in ["ast", "ir", "bytecode"] {
        let run = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(
            !run.status.success(),
            "extern call must fail on the {backend} interpreter"
        );
        let rendered = format!("{}{}", stdout(&run), stderr(&run));
        assert!(
            rendered.contains("L0423"),
            "expected L0423 on {backend}: {rendered}"
        );
    }

    ensure_msvc_env();

    // Native codegen: emit + link + run.
    let out = std::env::temp_dir().join("lullaby_ffi_ldexp.exe");
    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    assert!(
        stdout(&emit).contains("compiled main"),
        "expected `main` compiled: {}",
        stdout(&emit)
    );

    if rust_lld_path().is_none() || !kernel32_available() || !ucrt_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib/ucrt.lib (via the LIB env var) not available; \
             skipping mixed float/int C-ABI FFI link+run"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(exit, 12, "ldexp(1.5, 3)==12.0 via C FFI must exit 12");
}

/// Assert every interpreter backend rejects an extern-call fixture with `L0423`
/// (FFI is native-only), then native-compile it and assert `main` compiled. The
/// shared preamble for the pointer/cstr/many-arg FFI link+run tests below.
pub(crate) fn assert_ffi_native_only_and_compiles(fixture: &Path) {
    let check = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(check.status.success(), "{}", stderr(&check));

    for backend in ["ast", "ir", "bytecode"] {
        let run = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(
            !run.status.success(),
            "extern call must fail on the {backend} interpreter"
        );
        let rendered = format!("{}{}", stdout(&run), stderr(&run));
        assert!(
            rendered.contains("L0423"),
            "expected L0423 on {backend}: {rendered}"
        );
    }
}

/// C-ABI FFI (`cstr` string marshalling): `extern fn strlen s cstr -> usize` is
/// called with a Lullaby `string` literal `"lullaby"`. The FFI boundary
/// materializes a NUL-terminated UTF-8 copy (`__lullaby_to_cstr`) and passes its
/// `const char*` to the real C `strlen`, which returns 7 — so the `.exe` exits 7.
/// This proves a Lullaby `string` round-trips to C as a `char*`. On the
/// interpreters the extern call is `L0423`. Gated on `rust-lld` + `kernel32.lib` +
/// `ucrt.lib`; sources MSVC's `LIB` when unset so the link+run executes.
#[test]
pub(crate) fn ffi_cstr_marshals_string_to_c_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/native_only/ffi_cstr_strlen.lby");
    assert_ffi_native_only_and_compiles(&fixture);

    ensure_msvc_env();
    let out = std::env::temp_dir().join("lullaby_ffi_cstr_strlen.exe");
    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    assert!(
        stdout(&emit).contains("compiled main"),
        "expected `main` compiled: {}",
        stdout(&emit)
    );

    if rust_lld_path().is_none() || !kernel32_available() || !ucrt_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib/ucrt.lib not available; skipping cstr FFI link+run"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(exit, 7, "strlen(\"lullaby\") via cstr FFI must exit 7");
}

/// C-ABI FFI (raw pointer arguments/returns + round-trip): a Lullaby-controlled C
/// pointer round-trips through three C functions —
/// `malloc(16) -> ptr<byte>`, `strcpy(p, "hello") -> ptr<byte>` (a `cstr` source),
/// `strlen(p) -> usize`. `strlen` reads the buffer `strcpy` filled through the
/// `malloc`'d pointer, returning 5, so the `.exe` exits 5. This proves a
/// pointer alloc'd through C passes back into C by its raw machine address across
/// several calls. On the interpreters the extern call is `L0423`. Gated on
/// `rust-lld` + `kernel32.lib` + `ucrt.lib`; sources MSVC's `LIB` when unset.
#[test]
pub(crate) fn ffi_pointer_round_trips_through_c_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/native_only/ffi_ptr_roundtrip.lby");
    assert_ffi_native_only_and_compiles(&fixture);

    ensure_msvc_env();
    let out = std::env::temp_dir().join("lullaby_ffi_ptr_roundtrip.exe");
    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    assert!(
        stdout(&emit).contains("compiled main"),
        "expected `main` compiled: {}",
        stdout(&emit)
    );

    if rust_lld_path().is_none() || !kernel32_available() || !ucrt_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib/ucrt.lib not available; skipping pointer FFI link+run"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit, 5,
        "malloc+strcpy(\"hello\")+strlen round-trip must exit 5"
    );
}

/// C-ABI FFI (>4 extern arguments, Win64 stack spill): a caller object declares
/// `extern fn lullaby_sum6 a..f i64 -> i64` and calls it with six arguments
/// (`1+2+4+8+16+32 = 63`); a separate library object *exports* the same six-`i64`
/// function. Linking the two objects across the C ABI resolves the extern to the
/// export, so the `.exe` exits 63. This verifies the extern caller spills its 5th
/// and 6th arguments to the stack above the shadow space exactly where the export
/// callee reads them — end to end, without a C compiler (rust-lld links the two
/// Lullaby objects). On the interpreters the extern call is `L0423`. Gated on
/// `rust-lld` + `kernel32.lib` + `ucrt.lib`; sources MSVC's `LIB` when unset.
#[test]
pub(crate) fn ffi_extern_call_with_stack_args_when_linkable() {
    let caller = workspace_root().join("tests/fixtures/native_only/ffi_extern_sum6.lby");
    let callee = workspace_root().join("tests/fixtures/native_only/ffi_export_sum6.lby");

    // The caller (an extern call) is native-only and rejected by every interpreter.
    assert_ffi_native_only_and_compiles(&caller);

    // The callee is a C-callable library object (export only, no `main`); `check`
    // validates its export signature.
    let callee_check = lullaby()
        .args(["check", callee.to_str().expect("callee path")])
        .output()
        .expect("run cli");
    assert!(callee_check.status.success(), "{}", stderr(&callee_check));

    ensure_msvc_env();

    // Emit both objects. The CLI derives each `.obj` from the `-o` `.exe` stem and
    // writes it unconditionally (the caller's own self-link fails on the
    // unresolved export symbol, but the object is still produced — the reliable
    // floor).
    let caller_exe = std::env::temp_dir().join("lullaby_ffi_extern_sum6.exe");
    let callee_exe = std::env::temp_dir().join("lullaby_ffi_export_sum6.exe");
    let caller_obj = caller_exe.with_extension("obj");
    let callee_obj = callee_exe.with_extension("obj");
    let _ = std::fs::remove_file(&caller_obj);
    let _ = std::fs::remove_file(&callee_obj);
    for (src, exe) in [(&caller, &caller_exe), (&callee, &callee_exe)] {
        let emit = lullaby()
            .args([
                "native",
                "--verbose",
                "-o",
                exe.to_str().expect("out path"),
                src.to_str().expect("src path"),
            ])
            .output()
            .expect("run cli");
        assert!(emit.status.success(), "{}", stderr(&emit));
    }
    // The caller's `main` and the callee's six-parameter export both compile
    // natively (the stack-argument ABI keeps the >4-arg extern call in the subset).
    let caller_emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            caller_exe.to_str().expect("out path"),
            caller.to_str().expect("caller path"),
        ])
        .output()
        .expect("run cli");
    assert!(
        stdout(&caller_emit).contains("compiled main"),
        "expected caller `main` compiled: {}",
        stdout(&caller_emit)
    );
    assert!(caller_obj.is_file(), "expected caller object");
    assert!(callee_obj.is_file(), "expected callee object");

    if rust_lld_path().is_none() || !kernel32_available() || !ucrt_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib/ucrt.lib not available; skipping >4-arg extern link+run"
        );
        return;
    }

    // Link the two Lullaby objects into one executable. The caller supplies the
    // entry stub (`_lullaby_start`); the extern `lullaby_sum6` resolves to the
    // library object's exported symbol. `ucrt.lib` is on the line for the caller's
    // recorded C-runtime dependency (unused for this intra-Lullaby symbol).
    let lld = rust_lld_path().expect("rust-lld present (gate checked)");
    let linked = std::env::temp_dir().join("lullaby_ffi_sum6_linked.exe");
    let _ = std::fs::remove_file(&linked);
    let mut command = Command::new(&lld);
    command.args(["-flavor", "link", "/nologo", "/subsystem:console"]);
    command.arg("/entry:_lullaby_start");
    command.arg(format!("/out:{}", linked.display()));
    for dir in lib_dirs_from_env() {
        command.arg(format!("/libpath:{}", dir.display()));
    }
    command.arg(&caller_obj);
    command.arg(&callee_obj);
    command.arg("kernel32.lib");
    command.arg("ucrt.lib");
    let link = command.output().expect("run rust-lld");
    assert!(
        link.status.success(),
        "two-object link failed: {}{}",
        String::from_utf8_lossy(&link.stdout),
        String::from_utf8_lossy(&link.stderr)
    );

    assert!(
        linked.is_file(),
        "expected linked exe at {}",
        linked.display()
    );
    let exe = Command::new(&linked).output().expect("run linked exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit, 63,
        "lullaby_sum6(1,2,4,8,16,32) via a >4-arg C-ABI extern call must exit 63"
    );
}

/// The MSVC library search directories named by the `LIB` environment variable
/// (set in a Developer Command Prompt or by `ensure_msvc_env`). Used to build the
/// `/libpath:` arguments for a direct two-object `rust-lld` link.
pub(crate) fn lib_dirs_from_env() -> Vec<PathBuf> {
    std::env::var("LIB")
        .ok()
        .into_iter()
        .flat_map(|lib| {
            lib.split(';')
                .filter(|d| !d.is_empty())
                .map(|d| PathBuf::from(d.trim()))
                .collect::<Vec<_>>()
        })
        .filter(|d| d.is_dir())
        .collect()
}

/// Inline assembly: a `main` whose `unsafe` `asm` block emits the seven bytes of
/// `mov rax, 42` (`0x48,0xC7,0xC0,0x2A,0x00,0x00,0x00`). On the interpreters the
/// `asm` is rejected with `L0425` (raw machine code needs native codegen). Native-
/// compiled and linked, the emitted `mov rax, 42` reaches the Win64 epilogue, so
/// the process exits 42. Gated on `rust-lld` + `kernel32.lib`; skips gracefully.
#[test]
pub(crate) fn asm_emits_raw_bytes_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/native_only/asm_mov.lby");

    // `check` validates the asm shape (byte range + enclosing `unsafe`).
    let check = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(check.status.success(), "{}", stderr(&check));

    // Every interpreter backend rejects the `asm` with L0425.
    for backend in ["ast", "ir", "bytecode"] {
        let run = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(
            !run.status.success(),
            "asm must fail on the {backend} interpreter"
        );
        let rendered = format!("{}{}", stdout(&run), stderr(&run));
        assert!(
            rendered.contains("L0425"),
            "expected L0425 on {backend}: {rendered}"
        );
    }

    // Native codegen: emit + (best-effort) link + run.
    let out = std::env::temp_dir().join("lullaby_asm_mov.exe");
    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    assert!(
        stdout(&emit).contains("compiled main"),
        "expected `main` compiled: {}",
        stdout(&emit)
    );

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
             skipping inline-asm link+run"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(exit, 42, "asm `mov rax, 42` must make the process exit 42");
}

/// Whether the raw bytes of a native COFF object or linked PE image contain any
/// C-runtime dependency marker. A CRT-linked Windows image imports one of these
/// runtime DLLs (`ucrtbase`, `vcruntime*`, `msvcrt`, or an `api-ms-win-crt-*`
/// forwarder); a freestanding (kernel32-only) image imports none of them, and a
/// freestanding object carries no undefined external symbol from them either.
pub(crate) fn contains_crt_marker(bytes: &[u8]) -> Option<String> {
    // Case-insensitive substring scan over the ASCII import/symbol names embedded
    // in the object/image. These markers never appear in a kernel32-only build.
    const CRT_MARKERS: [&[u8]; 4] = [b"ucrt", b"vcruntime", b"msvcrt", b"api-ms-win-crt"];
    let lower: Vec<u8> = bytes.iter().map(|b| b.to_ascii_lowercase()).collect();
    for marker in CRT_MARKERS {
        if lower.windows(marker.len()).any(|w| w == marker) {
            return Some(String::from_utf8_lossy(marker).into_owned());
        }
    }
    None
}

/// Freestanding / no-std native build: `lullaby native --freestanding` must emit
/// an executable with NO C-runtime dependency — only the minimal OS import
/// (`kernel32!ExitProcess`) needed to terminate. This proves that end to end:
///
/// - The emitted object contains no CRT import/symbol marker (structural, always
///   runs). The only undefined external is `ExitProcess` (kernel32).
/// - When `rust-lld` + `kernel32.lib` are available, the linked `.exe` also
///   contains no CRT DLL import and its exit code equals the interpreter result
///   (mod 256), proving the kernel32-only image runs correctly.
///
/// Skips the link+run gracefully when the toolchain is unavailable, but always
/// runs the object-level no-CRT assertion.
#[test]
pub(crate) fn native_freestanding_has_no_crt_dependency_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_native_freestanding.exe");

    let emit = lullaby()
        .args([
            "native",
            "--freestanding",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    let listing = stdout(&emit);
    assert!(
        listing.contains("freestanding (no-std)"),
        "expected the freestanding no-CRT notice: {listing}"
    );
    assert!(
        listing.contains("compiled main"),
        "expected `main` compiled: {listing}"
    );

    // Structural (always runs): the emitted object has no C-runtime marker. The
    // only undefined external symbol is `ExitProcess` (from kernel32), which is
    // not a CRT dependency.
    let obj = out.with_extension("obj");
    let obj_bytes = std::fs::read(&obj).expect("read native object");
    if let Some(marker) = contains_crt_marker(&obj_bytes) {
        panic!("freestanding object must not reference the C runtime; found `{marker}`");
    }
    // Sanity: the object references `ExitProcess` (the minimal OS import).
    assert!(
        obj_bytes.windows(11).any(|w| w == b"ExitProcess"),
        "freestanding object should import kernel32!ExitProcess for process exit"
    );

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, 39, "fixture main computes 39");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
             skipping freestanding link+run parity (object-level no-CRT check already ran)"
        );
        return;
    }

    // The linked image must also carry no C-runtime import (kernel32-only), and
    // its exit code must match the interpreter result (mod 256).
    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe_bytes = std::fs::read(&out).expect("read linked exe");
    if let Some(marker) = contains_crt_marker(&exe_bytes) {
        panic!("freestanding exe must not import the C runtime; found `{marker}`");
    }
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        (interp.rem_euclid(256)) as i32,
        "freestanding native exit code must equal the interpreter result (mod 256)"
    );
}

/// A freestanding build that also declares an `extern fn` (which requires the C
/// runtime import library `ucrt.lib`) is a contradiction: `--freestanding`
/// guarantees no C runtime. The CLI rejects the combination with `L0426` rather
/// than silently linking the CRT.
#[test]
pub(crate) fn native_freestanding_rejects_extern_fn_with_l0426() {
    let fixture = workspace_root().join("tests/fixtures/native_only/ffi_llabs.lby");
    let output = lullaby()
        .args([
            "native",
            "--freestanding",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(
        !output.status.success(),
        "freestanding + extern fn must be rejected"
    );
    let rendered = format!("{}{}", stdout(&output), stderr(&output));
    assert!(rendered.contains("L0426"), "expected L0426: {rendered}");
    assert!(
        rendered.contains("ucrt.lib"),
        "diagnostic should name the offending C runtime import library: {rendered}"
    );
}

/// Discover a C compiler for the export-into-Lullaby execution test: prefer
/// MSVC `cl.exe` (present in a Developer Command Prompt, alongside `kernel32.lib`
/// on `LIB`), else `clang`. Returns the compiler program name when it runs.
pub(crate) fn find_c_compiler() -> Option<&'static str> {
    for candidate in ["cl", "clang"] {
        let ok = Command::new(candidate)
            .arg(if candidate == "cl" { "/?" } else { "--version" })
            .output()
            .map(|out| out.status.success() || candidate == "cl")
            .unwrap_or(false);
        if ok {
            return Some(candidate);
        }
    }
    None
}

/// C-calls-into-Lullaby FFI: an `export fn add_seven x i64 -> i64` is compiled to
/// a *library* COFF object (no `main`, no entry stub) whose `add_seven` symbol is
/// externally visible and defined in `.text`. A tiny C program declares
/// `extern long long add_seven(long long);`, calls it, and returns the result;
/// compiled and linked against the Lullaby object, the `.exe` exits with the
/// value `add_seven` computes. Gated on a discoverable C compiler; skips
/// gracefully otherwise (the object emission part always runs).
#[test]
pub(crate) fn c_calls_into_exported_lullaby_function_when_compilable() {
    let fixture = workspace_root().join("tests/fixtures/native_only/export_add_seven.lby");

    // `check` validates the export declaration and body (i64-scalar signature).
    let check = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(check.status.success(), "{}", stderr(&check));

    // Native codegen: emit the library object. `add_seven` compiles; there is no
    // `main`, so the CLI reports a C-callable library object rather than an exe.
    // The CLI derives the object path from the `-o` exe path (same stem, `.obj`).
    let exe_arg = std::env::temp_dir().join("lullaby_export_add_seven.exe");
    let obj = exe_arg.with_extension("obj");
    let _ = std::fs::remove_file(&obj);
    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            exe_arg.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    assert!(
        stdout(&emit).contains("compiled add_seven"),
        "expected `add_seven` compiled: {}",
        stdout(&emit)
    );
    assert!(
        stdout(&emit).contains("C-callable library object"),
        "expected a C-callable library object report: {}",
        stdout(&emit)
    );
    assert!(obj.is_file(), "expected object at {}", obj.display());

    let Some(cc) = find_c_compiler() else {
        eprintln!("no C compiler (cl/clang) found; skipping C-calls-into-Lullaby execution");
        return;
    };

    // A tiny C caller that calls the exported Lullaby function.
    let c_src = std::env::temp_dir().join("lullaby_export_caller.c");
    std::fs::write(
        &c_src,
        "extern long long add_seven(long long);\nint main(void){ return (int)add_seven(35); }\n",
    )
    .expect("write c caller");
    let out_exe = std::env::temp_dir().join("lullaby_export_caller.exe");
    let _ = std::fs::remove_file(&out_exe);

    let link = if cc == "cl" {
        // cl caller.c lullaby.obj /Fe:out.exe (MSVC driver links the CRT + obj).
        Command::new("cl")
            .args(["/nologo"])
            .arg(&c_src)
            .arg(&obj)
            .arg(format!("/Fe:{}", out_exe.display()))
            .current_dir(std::env::temp_dir())
            .output()
    } else {
        Command::new("clang")
            .arg(&c_src)
            .arg(&obj)
            .arg("-o")
            .arg(&out_exe)
            .output()
    };
    let link = match link {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            eprintln!(
                "C compiler `{cc}` could not link the export object; skipping run:\n{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        Err(error) => {
            eprintln!("could not run C compiler `{cc}`: {error}; skipping run");
            return;
        }
    };
    let _ = link;

    assert!(
        out_exe.is_file(),
        "expected linked exe at {}",
        out_exe.display()
    );
    let run = Command::new(&out_exe).output().expect("run c caller exe");
    let exit = run.status.code().expect("c caller exit code");
    // add_seven(35) == 42; the C `main` returns it as the process exit code.
    assert_eq!(
        exit, 42,
        "C caller into Lullaby `add_seven(35)` must exit 42"
    );
}

#[test]
pub(crate) fn test_runner_passes_on_demo_suite() {
    // The user-facing demo test suite has four `test_*` functions that all pass
    // via `assert`, with no `main`. `lullaby test` exits 0 and reports all pass.
    let demo = workspace_root().join("examples/valid/tests_demo/tests_demo.lby");
    let output = lullaby()
        .args(["test", demo.to_str().expect("demo path")])
        .output()
        .expect("run cli");
    let out = stdout(&output);
    assert!(output.status.success(), "{output:?}\n{out}");
    assert!(out.contains("PASS test_arith"), "{out}");
    assert!(out.contains("4 passed, 0 failed"), "{out}");
}

#[test]
pub(crate) fn test_runner_reports_failing_assert_and_exits_nonzero() {
    // A test that `assert(false)`s must fail: `lullaby test` prints FAIL with the
    // `assertion failed` message and exits non-zero.
    let tmp = std::env::temp_dir().join("lullaby_test_failing.lby");
    std::fs::write(
        &tmp,
        "fn test_passes -> void\n    assert(true)\n\nfn test_fails -> void\n    assert(false)\n",
    )
    .expect("write temp");
    let output = lullaby()
        .args(["test", tmp.to_str().expect("temp path")])
        .output()
        .expect("run cli");
    let out = stdout(&output);
    assert!(!output.status.success(), "{output:?}\n{out}");
    assert!(out.contains("PASS test_passes"), "{out}");
    assert!(out.contains("FAIL test_fails"), "{out}");
    assert!(out.contains("assertion failed"), "{out}");
    assert!(out.contains("1 passed, 1 failed"), "{out}");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
pub(crate) fn runs_project_manifest_across_backends() {
    // `project_demo` has a `lullaby.json` naming `src/main.lby` as its entry, its
    // own `src` module (`geometry`), and a local path dependency `mathx`. The
    // build resolves `import mathx`/`import geometry` across the project's and the
    // dependency's `src` directories and must produce 45 on every backend, whether
    // the argument is the project directory or the manifest path.
    let project = workspace_root().join("examples/valid/project_demo");
    let manifest = project.join("lullaby.json");
    for target in [&project, &manifest] {
        for backend in ["ast", "ir", "bytecode"] {
            let output = lullaby()
                .args([
                    "run",
                    "--backend",
                    backend,
                    target.to_str().expect("project path"),
                ])
                .output()
                .expect("run cli");
            assert!(output.status.success(), "{backend} {target:?}: {output:?}");
            assert_eq!(stdout(&output).trim(), "45", "{backend} {target:?}");
        }
    }
}

#[test]
pub(crate) fn checks_project_manifest() {
    let project = workspace_root().join("examples/valid/project_demo");
    let output = lullaby()
        .args(["check", project.to_str().expect("project path")])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("ok:"), "{}", stdout(&output));
}

#[test]
pub(crate) fn checks_library_project_without_entry() {
    // `mathx` is a library project (no `entry`): `check` validates every module.
    let project = workspace_root().join("examples/valid/mathx");
    let output = lullaby()
        .args(["check", project.to_str().expect("project path")])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("ok:"), "{}", stdout(&output));
}

#[test]
pub(crate) fn rejects_malformed_manifest_with_l0343() {
    let project = workspace_root().join("tests/fixtures/invalid/project_bad_manifest");
    let output = lullaby()
        .args(["check", project.to_str().expect("project path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(stderr.contains("L0343 [loader error]"), "{stderr}");
    assert!(stderr.contains("parse project manifest"), "{stderr}");
}

#[test]
pub(crate) fn rejects_missing_dependency_with_l0343() {
    let project = workspace_root().join("tests/fixtures/invalid/project_missing_dep");
    let output = lullaby()
        .args(["run", project.to_str().expect("project path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(stderr.contains("L0343 [loader error]"), "{stderr}");
    assert!(stderr.contains("ghost"), "{stderr}");
}

#[test]
pub(crate) fn rejects_cross_package_private_use_with_l0392() {
    // `app` imports the `libp` dependency and calls its private `hidden_helper`,
    // which is not visible across the package boundary.
    let project = workspace_root().join("tests/fixtures/invalid/project_private_cross/app");
    let output = lullaby()
        .args(["check", project.to_str().expect("project path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(stderr.contains("L0392 [loader error]"), "{stderr}");
    assert!(stderr.contains("hidden_helper"), "{stderr}");
}

/// A unique, empty scratch directory for a `lullaby new` test, cleaned first so
/// re-runs start fresh. Keyed by the test name to avoid collisions.
pub(crate) fn scratch_dir(key: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lullaby_new_test_{key}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[test]
pub(crate) fn new_scaffolds_a_runnable_project() {
    let work = scratch_dir("ok");
    let created = lullaby()
        .current_dir(&work)
        .args(["new", "bedtime"])
        .output()
        .expect("run cli");
    assert!(created.status.success(), "{created:?}");
    assert!(
        stdout(&created).contains("created bedtime/"),
        "{}",
        stdout(&created)
    );

    let root = work.join("bedtime");
    assert!(root.join("lullaby.json").is_file());
    assert!(root.join("src/main.lby").is_file());
    assert!(root.join(".gitignore").is_file());

    // The scaffold is a valid project the toolchain runs unmodified.
    let ran = lullaby()
        .current_dir(&work)
        .args(["run", "bedtime"])
        .output()
        .expect("run cli");
    assert!(ran.status.success(), "{ran:?}");
    assert!(
        stdout(&ran).contains("hello from bedtime"),
        "{}",
        stdout(&ran)
    );

    let _ = std::fs::remove_dir_all(&work);
}

#[test]
pub(crate) fn new_refuses_existing_directory() {
    let work = scratch_dir("exists");
    std::fs::create_dir(work.join("taken")).expect("pre-create dir");
    let output = lullaby()
        .current_dir(&work)
        .args(["new", "taken"])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "{output:?}");
    assert!(
        stderr(&output).contains("already exists"),
        "{}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
pub(crate) fn new_rejects_invalid_names() {
    let work = scratch_dir("invalid");
    for bad in ["my-app", "9lives", ""] {
        let output = lullaby()
            .current_dir(&work)
            .args(["new", bad])
            .output()
            .expect("run cli");
        assert!(!output.status.success(), "name {bad:?}: {output:?}");
    }
    // A rejected name creates nothing.
    assert!(!work.join("my-app").exists());
    let _ = std::fs::remove_dir_all(&work);
}

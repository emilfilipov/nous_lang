# Input/Output and Concurrency for Lullaby (lullaby)

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

## Overview

Input/output (I/O) and concurrency mechanisms in Lullaby are designed to be minimal, type-safe, and highly efficient. The design prioritizes simplicity for LLM comprehension while maintaining robustness for systems programming applications like operating system development.

## Current I/O And System Subset

The compiler implements a flat builtin surface — these names are available as ordinary function calls. The dotted `io.*` handle syntax, first-class stream handles, memory mapping, and `async`/`await` sketched in the design sections below are **planned and not implemented in that form**. The table below shows the core streaming/stdin builtins; the full shipped I/O surface (filesystem, process/environment, TCP/UDP sockets, and HTTP) lives in the flat builtins catalogued in [standard_library.md](standard_library.md) and [stdlib_surface_catalog.md](stdlib_surface_catalog.md). Lullaby *does* ship concurrency and networking as flat builtins (`spawn`/`task_join`/`mutex_*`/channels and `tcp_*`/`udp_*`/`http_*`) — they are simply not the handle-based, thread-pool, or socket-object shapes drafted further down.

| Builtin | Type | Behavior |
| :--- | :--- | :--- |
| `read_file(path)` | `string -> string` | Reads a UTF-8 text file into a `string`. Missing files, permission failures, and invalid paths are resource errors such as `L0414 [resource]: ...`. |
| `write_file(path, content)` | `string, string -> void` | Writes text to a file, replacing any existing file contents. |
| `append_file(path, content)` | `string, string -> void` | Appends text to a file, creating the file if it does not exist. |
| `file_exists(path)` | `string -> bool` | Returns whether the host can read metadata for the path. |
| `sys_status(program, args)` | `string, array<string> -> i64` | Runs `program` directly with an argv array and returns its exit status, or `-1` if the process terminates without a status code. |
| `sys_output(program, args)` | `string, array<string> -> string` | Runs `program` directly with an argv array and returns stdout as a string. |
| `read_line()` | `-> option<string>` | Reads one line from standard input with the trailing newline removed (a preceding `\r` from Windows CRLF input is removed too). Returns `none` at end-of-input; a blank input line is `some("")`, so EOF and an empty line stay distinct. Reads through the shared buffered stdin handle, so successive calls consume successive lines. |
| `read_all()` | `-> string` | Reads the whole of standard input to EOF into a single `string` (empty string when stdin is empty or already closed). |

`read_line`/`read_all` make Unix-style filter tools (`cat file \| tool`) expressible in Lullaby. They are interpreter-tier builtins (AST, IR, and bytecode); a function that calls one is not part of the native i64-scalar subset, so — like `read_file` and the other non-scalar builtins — it is cleanly skipped by `lullaby native`'s eligibility gate and runs on the interpreter, never crashing or producing a wrong result.

A cat-like echo filter that reads stdin line by line until end-of-input:

```lullaby
fn echo_next -> i64
    let line option<string> = read_line()
    match line
        some(text) -> emit(text)
        none -> 0 - 1

fn emit text string -> i64
    println(text)
    1

fn main -> i64
    let count i64 = 0
    loop
        let more i64 = echo_next()
        if more < 0
            break
        count += more
    count
```

System command builtins intentionally do not invoke a shell. Pass the executable name and arguments separately:

```lullaby
fn main -> i64
    sys_status("rustc", ["--version"])
```

Basic text file I/O:

```lullaby
fn main -> string
    write_file("target/example.txt", "alpha")
    append_file("target/example.txt", " beta")
    read_file("target/example.txt")
```

## Planned / Not-Yet-Implemented Design Material

> **Everything from here down is forward-looking design, not shipped surface.**
> The I/O that ships today is the flat builtin set in the
> [Current I/O And System Subset](#current-io-and-system-subset) section above,
> plus the filesystem, process/environment, TCP/UDP, and HTTP builtins catalogued
> in [standard_library.md](standard_library.md) and
> [stdlib_surface_catalog.md](stdlib_surface_catalog.md).
>
> The code samples in the sections below use **illustrative pseudocode that is not
> valid Lullaby**. Python-style `def` / `async def`, dotted `io.*` and
> `stream.method()` calls, keyword arguments (`size=4096`, `recursive=true`),
> `await` / `await_all`, and `end_region` / `end_stream` / `end_if` block
> terminators do **not** exist in the language. Lullaby is indentation-only (no
> `def`, no braces, no `end_*` terminators) and calls flat builtins by name
> (`read_file(path)`, not `io.read(...)`). Read the examples below as sketches of
> intent, not as compilable code.
>
> A few subsections are explicitly marked **"Delivered"** — those describe shipped
> behavior accurately (standard streams via `print`/`println`/`warn`/`flush` and
> stdin via `read_line`/`read_all`; non-blocking sockets via `set_nonblocking` plus
> the `*_nb` builtins). Everything not marked "Delivered" is planned.

## I/O System Design (planned)

### File Operations

Lullaby uses a simplified file I/O model that eliminates unnecessary boilerplate while maintaining safety:

#### Reading Files
```lullaby
# Read entire file into string

content = io.read("path/to/file.txt")

# Read file with specified line limit

lines = io.readlines("path/to/file.txt", max_lines=100)

# Stream reading (chunked processing)

stream = io.open("path/to/file.bin", mode="r")
while stream.has_more():
    chunk = stream.read_chunk(size=4096)
    process(chunk)
end_stream

# Read file with type inference

data = io.read_type("config.dat", target_type=config_struct)
```

#### Writing Files
```lullaby
# Write string to file

io.write("output.txt", "Hello, World!")

# Append to existing file

io.append("log.txt", "Entry at [timestamp]")

# Write binary data

io.write_binary("image.bin", raw_bytes)

# Atomic write (write then replace safely)

io.atomic_write("config.json", new_config_data)
```

#### File Information
```lullaby
# Get file metadata

meta = io.stat("path/to/file")
# Returns: size, modified_time, created_time, permissions, owner


# Check if file exists

if io.exists("config.ini"):
    load_default_settings()

# Read directory contents

files = io.list_dir("data/", recursive=true)
for file in files:
    process_file(file.name)
```

### Standard I/O Streams

**Delivered.** Standard input line/whole-stream reading is implemented as the flat
builtins `read_line() -> option<string>` and `read_all() -> string` (see the
current-subset table above), and standard output/error are the flat builtins
`print`/`println`/`warn`/`flush`. First-class stream handles with token-level
scanning and `has_more`-style iteration (the `input_stream.readline()` /
`read_token()` sketch below) remain planned design material layered on the same
streams.

```lullaby
# Input stream (keyboard/stdin) — planned handle-based surface

input_stream = io.stdin
while input_stream.has_more():
    line = input_stream.readline()
    token = input_stream.read_token()

# Output stream (terminal/stdout)

io.stdout.print("Processing...")
io.stderr.warn("Warning: low memory")
io.println(message)  # Line terminator included
```

### Memory Mapped Files

For large file processing without loading entirely into memory:
```lullaby
# Create memory-mapped region for file access

mm_file = io.memory_map("large_dataset.dat", size=1024*1024)

region mm_processing allocate
    data_ptr = mm_file.data_pointer

    # Direct memory access without copying
    for offset from 0 to file_size:
        value = *data_ptr[offset]

        if is_valid(value):
            process_record(offset, value)

end_region
```

## Concurrency System

### Threads and Processes (planned)

Lullaby provides lightweight concurrency primitives optimized for both performance and LLM comprehension:

#### Thread Creation and Management
```lullaby
# Create new thread with function reference

thread worker = spawn_thread(worker_function, arguments)

# Wait for thread completion

result = wait(thread)

# Thread synchronization

thread sync = create_mutex_sync()
lock(sync)
    shared_resource.modify()
unlock(sync)
end_lock

# Multiple threads coordination

sync_pool = create_thread_pool(size=4)
for i from 0 to num_tasks:
    task_data[i] = submit_task(sync_pool, task_function, params[i])

results = collect_task_results(sync_pool)
```

#### Process Management (Systems Programming)
```lullaby
# Spawn subprocess with command

process child = spawn_command("make", args=["clean"])
status = wait_process(child)

# Capture subprocess output

child_output = read_stream(process.stdout)
error_output = read_stream(process.stderr)

# Forward process output to file

pipe_to_file(process.stdout, "output.log")

# Check process exit code

if status.exit_code == 0:
    success_report()
else:
    error_handle(status.error_info)
end_if
```

### Asynchronous Operations (planned; no `async`/`await` in the language today)

Simple await/async pattern without complexity:
```lullaby
# Define async function

async def fetch_data(url):
    response = await http_get(url)
    if response.status == 200:
        data = await response.parse_json()
        return process(data)
    else:
        log_error(response.error)

# Use async function with await keyword

async def main():
    results = []

    urls = [url1, url2, url3]

    # Parallel execution
    tasks = []
    for url in urls:
        task = spawn_task(fetch_data, url)
        tasks.append(task)

    # Wait for all tasks to complete
    results = await_all(tasks)

    return combine_results(results)

main()  # Auto-runs if not explicitly awaited
```

### I/O Multiplexing (Non-blocking Operations)

**Delivered (std-only, portable).** Non-blocking socket I/O is implemented on the
AST, IR, and bytecode interpreters (the backends that hold live OS handles). A
socket is switched with `set_nonblocking(sock Socket, enabled bool) -> result<i64,
string>`, and the non-blocking accept/read/recv builtins surface a would-block
condition as `ok(none)` instead of blocking:

- `tcp_accept_nb(listener Socket) -> result<option<Socket>, string>`
- `tcp_read_nb(conn Socket, max i64) -> result<option<string>, string>`
- `udp_recv_nb(sock Socket) -> result<option<string>, string>`

`ok(some(v))` means data/connection is ready, `ok(none)` means it would block,
and `err(message)` is a real error. For `tcp_read_nb`, a 0-byte read (peer closed)
is `ok(some(""))`, matching blocking `tcp_read`. This is the correct std-only core
for an event loop: put sockets into non-blocking mode and poll the `*_nb` builtins
in a loop with a short backoff between empty passes, so one thread services many
sockets without blocking on any one. `set_nonblocking` + would-block-as-`none`
behave identically on Windows, Linux, and macOS through `std`. See
`documents/standard_library.md` (Networking) for the full signatures.

**Follow-up (post-1.0).** A `poll`/`select`-style readiness selector that parks
the calling thread until one of many sockets becomes ready — the epoll/kqueue/IOCP
multiplexer sketched below — is a deliberate follow-up. It avoids the
poll-with-backoff pattern but requires platform syscalls or an external crate, so
it is outside the std-only spanning set targeted for 1.0.

Efficient handling of multiple I/O operations (planned readiness-selector API):
```lullaby
# Create I/O event set

io_events = create_io_multiplexer(file_handles, socket_handles)

# Process events with timeout

while io_events.has_pending():
    ready_events = io_events.wait(timeout_ms=100)

    for event in ready_events:
        if is_readable(event):
            data = read_from(event)
            process_read(event, data)

        elif is_writable(event):
            write_to(event, buffer)

end_while
```

## Communication Mechanisms (planned)

### Inter-Process Communication (IPC)

#### Shared Memory
```lullaby
# Create shared memory region

shared_mem = create_shared_memory(size=1MB, name="data_pool")
shared_data = shared_mem.data_view()

# Multiple processes can access same memory safely

region process_a allocate
    local_ptr = get_pointer(shared_mem, offset)
    process_a_access(local_ptr)
end_region

region process_b allocate
    other_ptr = get_pointer(shared_mem, offset + 4096)
    process_b_access(other_ptr)
end_region
```

#### Message Queues
```lullaby
# Create message queue

msg_queue = create_message_channel(name="task_messages")
max_size = 1024

# Send message to queue

message = create_message(type=TASK, payload=data)
send_to_queue(msg_queue, message)

# Receive messages from queue

received = receive_from_queue(msg_queue, timeout_ms=500)
if received:
    process_message(received.content)
```

#### Socket Communication (Network I/O)
```lullaby
# Create TCP socket

socket client = io.socket_create(AF_INET, SOCK_STREAM)
status = connect(client, server_ip, port)

# Send data through socket

sent_bytes = send(client, request_data)

# Receive response from socket

response = receive(client, max_size=4096)
if is_valid(response):
    result = parse_response(response)
end_if

# Close socket properly

client.close()
```

### Inter-Thread Communication

```lullaby
# Thread-local message passing

thread_local_queue = create_thread_channel(thread_a, thread_b)

# Send from one thread to another

send_to_peer(thread_a.queue, data_message)

# Receive in receiving thread

received_msg = receive_from_peer(thread_b.queue, timeout_ms=100)
process_message(received_msg)
```

## Performance Optimization Strategies (planned)

### I/O Buffering
```lullaby
# Automatic buffering for sequential operations

stream_buffered = io.open("large_file.dat", buffered=true)

region large_processing allocate
    buffer_size = 65536

    # Read in chunks to minimize system calls
    while has_more_data():
        chunk = stream_buffered.read(buffer_size)

        if is_empty(chunk):
            break

        process_chunk(chunk)

        # Write output in batches
        batch_counter += len(chunk)
        if batch_counter >= flush_interval:
            io.stdout.flush(output_stream)
            batch_counter = 0
end_region
```

### Concurrency Optimization

#### Parallel Processing
```lullaby
# Divide work among multiple threads

task_distribution = partition_work(total_data, num_workers=4)

parallel_threads(4):
    for worker_id from 0 to 3:
        worker_task[worker_id] = spawn_thread(process_worker, task_distribution[worker_id])

results = collect_parallel_results(worker_tasks)
final_result = merge_results(results)
```

#### Pipeline Processing
```lullaby
# Create processing pipeline stages

pipeline = create_pipeline(
    stage1=input_validator,
    stage2=data_transformer,
    stage3=result_aggregator
)

# Execute pipeline with streaming I/O

stream input_data from source:
    validated = pipeline.process(input_data)

    if is_valid(validated):
        transformed = transform_stage(processed_data)

        aggregated_result = aggregate(transformed)
        output.write(aggregated_result)
end_stream
```

### Memory-Efficient Operations

#### Lazy Loading
```lullaby
# Load only necessary data portions

lazy_loader = create_lazy_file_loader("large_dataset.dat")

region streaming_processing allocate
    chunk_size = 4096

    while has_more_chunks():
        chunk = lazy_loader.load_next_chunk(chunk_size)

        process_chunk(chunk, context_state)
        update_context_state(processed_results)

end_region
```

## Design Principles Summary (planned design goals)

_These summarize the goals of the planned design above, not shipped behavior. For what ships today see the Current I/O And System Subset section and standard_library.md._

### I/O System Advantages
1. **Minimal Syntax**: Single keywords for common operations (read/write/open/close)
2. **Type Safety**: Automatic type inference prevents buffer overflows and format mismatches
3. **Automatic Buffering**: Optimizes performance without manual memory management
4. **Context Awareness**: Stream handling knows current read/write positions automatically

### Concurrency System Advantages
1. **Flat Structure**: No complex thread state machines required
2. **Type-Safe Operations**: Prevents race conditions through compile-time checking
3. **Automatic Synchronization**: Locks managed automatically, no manual deadlock prevention needed
4. **Unified Model**: Same syntax for threads, processes, and async operations

### LLM Optimization Benefits
1. **Predictable Patterns**: Simple if/then rules for concurrency control
2. **Limited State**: Threads represented as references rather than complex objects
3. **Clear Boundaries**: Scope-based region management for resource cleanup
4. **Minimal Keywords**: 5-7 core operations cover all common scenarios

## Example: Complete I/O + Concurrency Application

```lullaby
region os_kernel allocate

    # Multi-threaded file processor

    def process_directory(directory_path):
        files = io.list_dir(directory_path, recursive=true)

        thread_pool = create_thread_pool(size=8)

        for file in files:
            if is_file(file.type):
                task = spawn_task(
                    file_processor.process_single_file,
                    record file: file, pool_id: next available
                )

        results = collect_tasks_from_pool(thread_pool)

        sorted_results = sort(results, key=lambda r: r.file_size)

        return aggregated_statistics(sorted_results)

    # Network server handler

    async def handle_client_request(client_socket, request):
        response_data = await client_socket.send(request)

        if response_data.success:
            response_status = parse_response(response_data)
            log_success(client_socket.id, response_status.code)

            metrics.update_server_stats(
                active_connections=decrement_active_count(),
                requests_processed=increase_request_count()
            )

            await client_socket.close()
        else:
            error_info = handle_error_response(response_data.error)
            log_warning(client_socket.id, error_info.message)
            metrics.increment_failure_rate()

    # Main server loop

    def start_server(listen_addr):
        sockets = create_multiple_sockets(
            family=AF_INET,
            type=SOCK_STREAM,
            count=max_concurrent_connections
        )

        active_connections = init_connection_table(size=max_concurrent_connections)

        while is_server_running:
            ready_events = io_wait_for_events(
                sockets,
                timeout_ms=1000,
                interest=readable_writable
            )

            for socket_event in ready_events:
                if is_readable(socket_event.socket):
                    client_socket = socket_event.socket

                    request_data = read_from_client(client_socket)

                    task_thread = spawn_thread(
                        handle_client_request,
                        record socket: client_socket, request: request_data
                    )

                    active_connections[client_socket.id] = task_thread

                elif is_writable(socket_event.socket):
                    socket_event.socket.close()

end_region
```

## Summary (planned design)

_This recaps the planned design's intended benefits, not the current shipped surface. The `async`/`await`, thread-pool, and IPC features named below are not implemented today; the shipped I/O is the flat builtin set documented above and in standard_library.md._

The I/O and concurrency system in lullaby aims to provide:
- **Minimal, readable syntax** for file operations without boilerplate code
- **Type-safe access** preventing common errors like buffer overflows
- **Simple async model** using single `await` keyword instead of complex state machines
- **Efficient thread management** through reference-based synchronization primitives
- **Integrated IPC mechanisms** for shared memory, message queues, and sockets
- **Performance optimization** through automatic buffering and intelligent chunking

This design enables writing robust systems programs with minimal code complexity while maintaining high performance suitable for operating system development. The flat structure and reduced token requirements make it particularly well-suited for generation by small language models.

fn cleanup ptr ptr_i64 -> void
    dealloc(ptr)

fn main -> i64
    let ptr ptr_i64 = alloc(42)
    store(ptr, 42)
    let value i64 = load(ptr)
    cleanup(ptr)
    value

fn main -> i64
    let ptr ptr_i64 = alloc(0)
    store(ptr, 41)
    let value i64 = load(ptr)
    dealloc(ptr)
    value + 1

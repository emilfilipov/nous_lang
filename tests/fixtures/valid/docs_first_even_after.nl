fn first_even_after start i64 -> i64
    let value i64 = start
    loop
        value += 1
        if value == 3
            continue
        if value == 4
            break
    value

// Cross-language parsing suite (Rust). Real-world parsing over strings and i64
// arrays mirroring ../lullaby.lby. Inputs are `&str`; `eval_rpn` takes an i64 slice
// plus a length. Invalid numeric input returns -1 (or 0 for the signed parser,
// whose full range includes -1). See ../SPEC.md.
#![allow(dead_code)]

fn parse_uint(s: &str) -> i64 {
    let b = s.as_bytes();
    if b.is_empty() {
        return -1;
    }
    let mut val: i64 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return -1;
        }
        val = val * 10 + (c - b'0') as i64;
    }
    val
}

fn parse_int_signed(s: &str) -> i64 {
    let b = s.as_bytes();
    if b.is_empty() {
        return 0;
    }
    let (neg, start) = if b[0] == b'-' { (true, 1) } else { (false, 0) };
    if start == b.len() {
        return 0;
    }
    let mut val: i64 = 0;
    for &c in &b[start..] {
        if !c.is_ascii_digit() {
            return 0;
        }
        val = val * 10 + (c - b'0') as i64;
    }
    if neg {
        -val
    } else {
        val
    }
}

fn is_valid_int(s: &str) -> i64 {
    let b = s.as_bytes();
    if b.is_empty() {
        return 0;
    }
    let start = if b[0] == b'-' { 1 } else { 0 };
    if start == b.len() {
        return 0;
    }
    if b[start..].iter().all(|c| c.is_ascii_digit()) {
        1
    } else {
        0
    }
}

fn count_fields(s: &str, sep: &str) -> i64 {
    let target = sep.as_bytes()[0];
    s.as_bytes().iter().filter(|&&c| c == target).count() as i64 + 1
}

fn nth_field_len(s: &str, sep: &str, nth: i64) -> i64 {
    let target = sep.as_bytes()[0];
    let mut field: i64 = 0;
    let mut cur: i64 = 0;
    let mut result: i64 = -1;
    for &c in s.as_bytes() {
        if c == target {
            if field == nth {
                result = cur;
            }
            field += 1;
            cur = 0;
        } else {
            cur += 1;
        }
    }
    if field == nth {
        result = cur;
    }
    result
}

fn count_lines(s: &str) -> i64 {
    if s.is_empty() {
        return 0;
    }
    s.as_bytes().iter().filter(|&&c| c == b'\n').count() as i64 + 1
}

fn strip_leading_zeros_len(s: &str) -> i64 {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i] == b'0' {
        i += 1;
    }
    (b.len() - i) as i64
}

fn eval_rpn(tokens: &[i64], n: i64) -> i64 {
    if n == 0 {
        return 0;
    }
    let mut stack: Vec<i64> = Vec::with_capacity(n as usize);
    for &t in &tokens[..n as usize] {
        if t >= 0 {
            stack.push(t);
        } else {
            let b = stack.pop().unwrap();
            let a = stack.pop().unwrap();
            let r = match -t {
                1 => a + b,
                2 => a - b,
                3 => a * b,
                _ => a / b,
            };
            stack.push(r);
        }
    }
    stack[0]
}

fn count_digits_in(s: &str) -> i64 {
    s.as_bytes().iter().filter(|c| c.is_ascii_digit()).count() as i64
}

fn count_words(s: &str) -> i64 {
    let mut count: i64 = 0;
    let mut in_word = false;
    for &c in s.as_bytes() {
        if c == b' ' || c == b'\t' || c == b'\n' {
            in_word = false;
        } else if !in_word {
            in_word = true;
            count += 1;
        }
    }
    count
}

fn hex_to_int(s: &str) -> i64 {
    let b = s.as_bytes();
    if b.is_empty() {
        return -1;
    }
    let mut val: i64 = 0;
    for &c in b {
        let d: i64 = match c {
            b'0'..=b'9' => (c - b'0') as i64,
            b'a'..=b'f' => (c - b'a') as i64 + 10,
            b'A'..=b'F' => (c - b'A') as i64 + 10,
            _ => return -1,
        };
        val = val * 16 + d;
    }
    val
}

fn bin_to_int(s: &str) -> i64 {
    let b = s.as_bytes();
    if b.is_empty() {
        return -1;
    }
    let mut val: i64 = 0;
    for &c in b {
        match c {
            b'0' => val *= 2,
            b'1' => val = val * 2 + 1,
            _ => return -1,
        }
    }
    val
}

fn roman_value(c: u8) -> i64 {
    match c {
        b'I' => 1,
        b'V' => 5,
        b'X' => 10,
        b'L' => 50,
        b'C' => 100,
        b'D' => 500,
        b'M' => 1000,
        _ => 0,
    }
}

fn roman_to_int(s: &str) -> i64 {
    let b = s.as_bytes();
    let n = b.len();
    let mut total: i64 = 0;
    for i in 0..n {
        let v = roman_value(b[i]);
        if i + 1 < n && v < roman_value(b[i + 1]) {
            total -= v;
        } else {
            total += v;
        }
    }
    total
}

fn char_class_count(s: &str) -> i64 {
    s.as_bytes().iter().filter(|c| c.is_ascii_alphabetic()).count() as i64
}

fn main() {
    let rpn: [i64; 5] = [3, 4, -1, 5, -3];
    assert_eq!(parse_uint("01234"), 1234);
    assert_eq!(parse_int_signed("-42"), -42);
    assert_eq!(is_valid_int("-42"), 1);
    assert_eq!(count_fields("a,b,c,d", ","), 4);
    assert_eq!(nth_field_len("a,bb,ccc", ",", 2), 3);
    assert_eq!(count_lines("a\nb"), 2);
    assert_eq!(strip_leading_zeros_len("00042"), 2);
    assert_eq!(eval_rpn(&rpn, 5), 35);
    assert_eq!(count_digits_in("ab12cd34"), 4);
    assert_eq!(count_words("the quick brown fox"), 4);
    assert_eq!(hex_to_int("1a2f"), 6703);
    assert_eq!(bin_to_int("101101"), 45);
    assert_eq!(roman_to_int("MCMXCIV"), 1994);
    assert_eq!(char_class_count("abc123XYZ"), 6);
    println!("ok");
}

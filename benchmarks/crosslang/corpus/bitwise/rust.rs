// Cross-language bitwise suite (Rust). Bit manipulation over i64 done with
// ARITHMETIC only: no bitwise operators (^ & | << >>) anywhere, to stay
// algorithm-identical to the Lullaby port, which models bit operations with arithmetic.
// Bits are read with % 2 and shifted with / 2 (or * a power of two).

fn pow2(n: i64) -> i64 {
    let mut p = 1;
    for _ in 1..=n {
        p *= 2;
    }
    p
}

fn count_set_bits(mut x: i64) -> i64 {
    let mut count = 0;
    while x > 0 {
        count += x % 2;
        x /= 2;
    }
    count
}

fn is_power_of_two(x: i64) -> i64 {
    if x <= 0 {
        return 0;
    }
    if count_set_bits(x) == 1 { 1 } else { 0 }
}

fn highest_bit_pos(mut x: i64) -> i64 {
    let mut pos = -1;
    while x > 0 {
        pos += 1;
        x /= 2;
    }
    pos
}

fn lowest_bit_pos(mut x: i64) -> i64 {
    if x == 0 {
        return -1;
    }
    let mut pos = 0;
    while x % 2 == 0 {
        pos += 1;
        x /= 2;
    }
    pos
}

fn bit_at(x: i64, i: i64) -> i64 {
    (x / pow2(i)) % 2
}

fn extract_bits(x: i64, start: i64, count: i64) -> i64 {
    (x / pow2(start)) % pow2(count)
}

fn set_bit(x: i64, i: i64) -> i64 {
    if bit_at(x, i) == 1 {
        return x;
    }
    x + pow2(i)
}

fn clear_bit(x: i64, i: i64) -> i64 {
    if bit_at(x, i) == 0 {
        return x;
    }
    x - pow2(i)
}

fn toggle_bit(x: i64, i: i64) -> i64 {
    if bit_at(x, i) == 1 {
        return x - pow2(i);
    }
    x + pow2(i)
}

fn popcount_range(lo: i64, hi: i64) -> i64 {
    let mut total = 0;
    for v in lo..=hi {
        total += count_set_bits(v);
    }
    total
}

fn reverse_bits_n(mut x: i64, n: i64) -> i64 {
    let mut result = 0;
    for _ in 0..n {
        result = result * 2 + x % 2;
        x /= 2;
    }
    result
}

fn hamming_distance_bits(mut a: i64, mut b: i64) -> i64 {
    let mut dist = 0;
    while a > 0 || b > 0 {
        if a % 2 != b % 2 {
            dist += 1;
        }
        a /= 2;
        b /= 2;
    }
    dist
}

fn next_power_of_two(x: i64) -> i64 {
    let mut p = 1;
    while p < x {
        p *= 2;
    }
    p
}

fn count_leading_zeros_64(mut x: i64) -> i64 {
    if x == 0 {
        return 64;
    }
    let mut count = 64;
    while x > 0 {
        count -= 1;
        x /= 2;
    }
    count
}

fn parity_bit(x: i64) -> i64 {
    count_set_bits(x) % 2
}

fn is_bit_palindrome(x: i64, n: i64) -> i64 {
    if reverse_bits_n(x, n) == extract_bits(x, 0, n) { 1 } else { 0 }
}

fn rotate_left_n(mut x: i64, mut n: i64, bits: i64) -> i64 {
    let width = pow2(bits);
    x %= width;
    n %= bits;
    let hi = (x * pow2(n)) % width;
    let lo = x / pow2(bits - n);
    hi + lo
}

fn merge_bits(a: i64, b: i64, mask_count: i64) -> i64 {
    let m = pow2(mask_count);
    a - a % m + b % m
}

fn main() {
    println!("count_set_bits={}", count_set_bits(181));
    println!("is_power_of_two={}", is_power_of_two(64));
    println!("highest_bit_pos={}", highest_bit_pos(181));
    println!("lowest_bit_pos={}", lowest_bit_pos(180));
    println!("bit_at={}", bit_at(181, 2));
    println!("set_bit={}", set_bit(181, 1));
    println!("clear_bit={}", clear_bit(181, 0));
    println!("toggle_bit={}", toggle_bit(181, 3));
    println!("popcount_range={}", popcount_range(0, 7));
    println!("reverse_bits_n={}", reverse_bits_n(13, 4));
    println!("hamming_distance_bits={}", hamming_distance_bits(181, 90));
    println!("next_power_of_two={}", next_power_of_two(100));
    println!("count_leading_zeros_64={}", count_leading_zeros_64(1));
    println!("parity_bit={}", parity_bit(181));
    println!("is_bit_palindrome={}", is_bit_palindrome(9, 4));
    println!("rotate_left_n={}", rotate_left_n(1, 1, 4));
    println!("extract_bits={}", extract_bits(181, 2, 3));
    println!("merge_bits={}", merge_bits(240, 15, 4));
}

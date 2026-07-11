// Cross-language scalar function suite (Rust). Pure i64, matching ../lullaby/scalar.lby.
// `/` and `%` are truncating (match the reference `rem`); benchmark inputs are non-negative.
// Multiplying/accumulating functions use wrapping arithmetic so large inputs don't panic in debug.
#![allow(dead_code)]

fn add(a: i64, b: i64) -> i64 {
    a.wrapping_add(b)
}

fn max2(a: i64, b: i64) -> i64 {
    if a > b {
        a
    } else {
        b
    }
}

fn abs_val(n: i64) -> i64 {
    if n < 0 {
        -n
    } else {
        n
    }
}

fn is_even(n: i64) -> i64 {
    if n % 2 == 0 {
        1
    } else {
        0
    }
}

fn clamp(x: i64, lo: i64, hi: i64) -> i64 {
    if x < lo {
        return lo;
    }
    if x > hi {
        return hi;
    }
    x
}

fn sign(n: i64) -> i64 {
    if n < 0 {
        return -1;
    }
    if n > 0 {
        return 1;
    }
    0
}

fn factorial(n: i64) -> i64 {
    let mut r: i64 = 1;
    let mut i: i64 = 2;
    while i <= n {
        r = r.wrapping_mul(i);
        i += 1;
    }
    r
}

fn gcd(mut a: i64, mut b: i64) -> i64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

fn fib_iter(n: i64) -> i64 {
    let mut a: i64 = 0;
    let mut b: i64 = 1;
    let mut i: i64 = 0;
    while i < n {
        let t = a.wrapping_add(b);
        a = b;
        b = t;
        i += 1;
    }
    a
}

fn is_prime(n: i64) -> i64 {
    if n < 2 {
        return 0;
    }
    let mut d: i64 = 2;
    while d * d <= n {
        if n % d == 0 {
            return 0;
        }
        d += 1;
    }
    1
}

fn int_pow(base: i64, exp: i64) -> i64 {
    let mut r: i64 = 1;
    let mut i: i64 = 0;
    while i < exp {
        r = r.wrapping_mul(base);
        i += 1;
    }
    r
}

fn collatz_len(mut n: i64) -> i64 {
    let mut steps: i64 = 0;
    while n != 1 {
        if n % 2 == 0 {
            n /= 2;
        } else {
            n = 3 * n + 1;
        }
        steps += 1;
    }
    steps
}

fn digit_sum(mut n: i64) -> i64 {
    if n < 0 {
        n = -n;
    }
    let mut s: i64 = 0;
    while n > 0 {
        s += n % 10;
        n /= 10;
    }
    s
}

fn count_primes_below(n: i64) -> i64 {
    let mut count: i64 = 0;
    let mut k: i64 = 2;
    while k < n {
        if is_prime(k) == 1 {
            count += 1;
        }
        k += 1;
    }
    count
}

fn power_mod(mut base: i64, mut exp: i64, m: i64) -> i64 {
    let mut r: i64 = 1;
    base %= m;
    while exp > 0 {
        if exp % 2 == 1 {
            r = r.wrapping_mul(base) % m;
        }
        exp /= 2;
        base = base.wrapping_mul(base) % m;
    }
    r
}

fn ackermann(m: i64, n: i64) -> i64 {
    if m == 0 {
        return n + 1;
    }
    if n == 0 {
        return ackermann(m - 1, 1);
    }
    ackermann(m - 1, ackermann(m, n - 1))
}

fn main() {
    assert_eq!(add(2, 3), 5);
    assert_eq!(max2(3, 7), 7);
    assert_eq!(max2(5, 5), 5);
    assert_eq!(abs_val(-4), 4);
    assert_eq!(abs_val(0), 0);
    assert_eq!(is_even(4), 1);
    assert_eq!(is_even(7), 0);
    assert_eq!(clamp(5, 0, 3), 3);
    assert_eq!(clamp(-1, 0, 3), 0);
    assert_eq!(clamp(2, 0, 3), 2);
    assert_eq!(sign(-9), -1);
    assert_eq!(sign(0), 0);
    assert_eq!(sign(9), 1);
    assert_eq!(factorial(5), 120);
    assert_eq!(factorial(0), 1);
    assert_eq!(gcd(48, 18), 6);
    assert_eq!(gcd(7, 0), 7);
    assert_eq!(fib_iter(10), 55);
    assert_eq!(fib_iter(0), 0);
    assert_eq!(is_prime(1), 0);
    assert_eq!(is_prime(2), 1);
    assert_eq!(is_prime(97), 1);
    assert_eq!(is_prime(100), 0);
    assert_eq!(int_pow(2, 10), 1024);
    assert_eq!(int_pow(5, 0), 1);
    assert_eq!(collatz_len(1), 0);
    assert_eq!(collatz_len(6), 8);
    assert_eq!(digit_sum(1234), 10);
    assert_eq!(digit_sum(-90), 9);
    assert_eq!(count_primes_below(100), 25);
    assert_eq!(power_mod(7, 256, 13), 9);
    assert_eq!(ackermann(2, 3), 9);
    println!("ok");
}

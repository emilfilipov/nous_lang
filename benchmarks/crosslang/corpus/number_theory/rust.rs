// Cross-language number theory suite (Rust). Classic integer number theory.

fn gcd(mut a: i64, mut b: i64) -> i64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

fn int_pow(base: i64, exp: i64) -> i64 {
    let mut r: i64 = 1;
    for _ in 1..=exp {
        r = r.wrapping_mul(base);
    }
    r
}

fn num_digits(mut n: i64) -> i64 {
    if n == 0 {
        return 1;
    }
    let mut c = 0;
    while n > 0 {
        c += 1;
        n /= 10;
    }
    c
}

fn lcm(a: i64, b: i64) -> i64 {
    a / gcd(a, b) * b
}

fn divisor_count(n: i64) -> i64 {
    let mut count = 0;
    let mut d = 1;
    while d * d <= n {
        if n % d == 0 {
            count += 1;
            if d != n / d {
                count += 1;
            }
        }
        d += 1;
    }
    count
}

fn divisor_sum(n: i64) -> i64 {
    let mut sum = 0;
    let mut d = 1;
    while d * d <= n {
        if n % d == 0 {
            sum += d;
            let other = n / d;
            if other != d {
                sum += other;
            }
        }
        d += 1;
    }
    sum
}

fn is_perfect(n: i64) -> i64 {
    if divisor_sum(n) - n == n {
        1
    } else {
        0
    }
}

fn euler_totient(mut n: i64) -> i64 {
    let mut result = n;
    let mut p = 2;
    while p * p <= n {
        if n % p == 0 {
            while n % p == 0 {
                n /= p;
            }
            result -= result / p;
        }
        p += 1;
    }
    if n > 1 {
        result -= result / n;
    }
    result
}

fn count_coprime_below(n: i64) -> i64 {
    let mut count = 0;
    for k in 1..=n - 1 {
        if gcd(k, n) == 1 {
            count += 1;
        }
    }
    count
}

fn digital_root(mut n: i64) -> i64 {
    while n >= 10 {
        let mut s = 0;
        while n > 0 {
            s += n % 10;
            n /= 10;
        }
        n = s;
    }
    n
}

fn is_armstrong(n: i64) -> i64 {
    let d = num_digits(n);
    let mut sum = 0;
    let mut m = n;
    while m > 0 {
        sum += int_pow(m % 10, d);
        m /= 10;
    }
    if sum == n {
        1
    } else {
        0
    }
}

fn reverse_digits(mut n: i64) -> i64 {
    let mut r = 0;
    while n > 0 {
        r = r * 10 + n % 10;
        n /= 10;
    }
    r
}

fn is_palindrome_number(n: i64) -> i64 {
    if reverse_digits(n) == n {
        1
    } else {
        0
    }
}

fn sum_of_squares_digits(mut n: i64) -> i64 {
    let mut sum = 0;
    while n > 0 {
        let d = n % 10;
        sum += d * d;
        n /= 10;
    }
    sum
}

fn is_happy(mut n: i64) -> i64 {
    let mut steps = 0;
    while n != 1 && steps < 1000 {
        n = sum_of_squares_digits(n);
        steps += 1;
    }
    if n == 1 {
        1
    } else {
        0
    }
}

fn to_base_digit_sum(mut n: i64, b: i64) -> i64 {
    let mut sum = 0;
    while n > 0 {
        sum += n % b;
        n /= b;
    }
    sum
}

fn count_trailing_zeros_factorial(n: i64) -> i64 {
    let mut count = 0;
    let mut power = 5;
    while power <= n {
        count += n / power;
        power *= 5;
    }
    count
}

fn gcd_of_range(lo: i64, hi: i64) -> i64 {
    let mut g = lo;
    for k in lo..=hi {
        g = gcd(g, k);
    }
    g
}

fn main() {
    println!("lcm(12,18)={}", lcm(12, 18));
    println!("divisor_count(36)={}", divisor_count(36));
    println!("divisor_sum(36)={}", divisor_sum(36));
    println!("is_perfect(28)={}", is_perfect(28));
    println!("euler_totient(36)={}", euler_totient(36));
    println!("count_coprime_below(36)={}", count_coprime_below(36));
    println!("digital_root(9875)={}", digital_root(9875));
    println!("is_armstrong(153)={}", is_armstrong(153));
    println!("reverse_digits(1234)={}", reverse_digits(1234));
    println!("is_palindrome_number(1221)={}", is_palindrome_number(1221));
    println!("sum_of_squares_digits(123)={}", sum_of_squares_digits(123));
    println!("is_happy(19)={}", is_happy(19));
    println!("to_base_digit_sum(255,16)={}", to_base_digit_sum(255, 16));
    println!("count_trailing_zeros_factorial(100)={}", count_trailing_zeros_factorial(100));
    println!("gcd_of_range(12,24)={}", gcd_of_range(12, 24));
}

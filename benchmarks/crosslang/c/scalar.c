// Cross-language scalar function suite (C). Pure long long (i64) — the
// idiomatic-but-minimal C baseline for the token/perf comparison. See ../SPEC.md.
// C's `/` and `%` are truncating, matching the reference's `rem`.

#include <assert.h>
#include <stdio.h>

typedef long long i64;

i64 add(i64 a, i64 b) {
    return a + b;
}

i64 max2(i64 a, i64 b) {
    return a > b ? a : b;
}

i64 abs_val(i64 n) {
    return n < 0 ? -n : n;
}

i64 is_even(i64 n) {
    return n % 2 == 0 ? 1 : 0;
}

i64 clamp(i64 x, i64 lo, i64 hi) {
    if (x < lo) return lo;
    if (x > hi) return hi;
    return x;
}

i64 sign(i64 n) {
    if (n < 0) return -1;
    if (n > 0) return 1;
    return 0;
}

i64 factorial(i64 n) {
    i64 r = 1;
    for (i64 i = 2; i <= n; i++)
        r *= i;
    return r;
}

i64 gcd(i64 a, i64 b) {
    while (b != 0) {
        i64 t = a % b;
        a = b;
        b = t;
    }
    return a;
}

i64 fib_iter(i64 n) {
    i64 a = 0, b = 1;
    for (i64 i = 0; i < n; i++) {
        i64 t = a + b;
        a = b;
        b = t;
    }
    return a;
}

i64 is_prime(i64 n) {
    if (n < 2) return 0;
    for (i64 d = 2; d * d <= n; d++)
        if (n % d == 0) return 0;
    return 1;
}

i64 int_pow(i64 base, i64 exp) {
    i64 r = 1;
    for (i64 i = 0; i < exp; i++)
        r *= base;
    return r;
}

i64 collatz_len(i64 n) {
    i64 steps = 0;
    while (n != 1) {
        if (n % 2 == 0)
            n = n / 2;
        else
            n = 3 * n + 1;
        steps++;
    }
    return steps;
}

i64 digit_sum(i64 n) {
    if (n < 0) n = -n;
    i64 s = 0;
    while (n > 0) {
        s += n % 10;
        n /= 10;
    }
    return s;
}

i64 count_primes_below(i64 n) {
    i64 count = 0;
    for (i64 k = 2; k < n; k++)
        if (is_prime(k) == 1) count++;
    return count;
}

i64 power_mod(i64 base, i64 exp, i64 m) {
    i64 r = 1;
    base = base % m;
    while (exp > 0) {
        if (exp % 2 == 1)
            r = (r * base) % m;
        exp /= 2;
        base = (base * base) % m;
    }
    return r;
}

i64 ackermann(i64 m, i64 n) {
    if (m == 0) return n + 1;
    if (n == 0) return ackermann(m - 1, 1);
    return ackermann(m - 1, ackermann(m, n - 1));
}

int main(void) {
    assert(add(2, 3) == 5);
    assert(max2(3, 7) == 7);
    assert(max2(5, 5) == 5);
    assert(abs_val(-4) == 4);
    assert(abs_val(0) == 0);
    assert(is_even(4) == 1);
    assert(is_even(7) == 0);
    assert(clamp(5, 0, 3) == 3);
    assert(clamp(-1, 0, 3) == 0);
    assert(clamp(2, 0, 3) == 2);
    assert(sign(-9) == -1);
    assert(sign(0) == 0);
    assert(sign(9) == 1);
    assert(factorial(5) == 120);
    assert(factorial(0) == 1);
    assert(gcd(48, 18) == 6);
    assert(gcd(7, 0) == 7);
    assert(fib_iter(10) == 55);
    assert(fib_iter(0) == 0);
    assert(is_prime(1) == 0);
    assert(is_prime(2) == 1);
    assert(is_prime(97) == 1);
    assert(is_prime(100) == 0);
    assert(int_pow(2, 10) == 1024);
    assert(int_pow(5, 0) == 1);
    assert(collatz_len(1) == 0);
    assert(collatz_len(6) == 8);
    assert(digit_sum(1234) == 10);
    assert(digit_sum(-90) == 9);
    assert(count_primes_below(100) == 25);
    assert(power_mod(7, 256, 13) == 9);
    assert(ackermann(2, 3) == 9);
    printf("ok\n");
    return 0;
}

// Cross-language scalar function suite (C++). Mirrors ../lullaby/scalar.lby.
// All functions take/return long long; / and % are truncating (C's semantics).
#include <cassert>
#include <iostream>

long long add(long long a, long long b) {
    return a + b;
}

long long max2(long long a, long long b) {
    return a > b ? a : b;
}

long long abs_val(long long n) {
    return n < 0 ? -n : n;
}

long long is_even(long long n) {
    return n % 2 == 0 ? 1 : 0;
}

long long clamp(long long x, long long lo, long long hi) {
    if (x < lo) return lo;
    if (x > hi) return hi;
    return x;
}

long long sign(long long n) {
    if (n < 0) return -1;
    if (n > 0) return 1;
    return 0;
}

long long factorial(long long n) {
    long long r = 1;
    for (long long i = 2; i <= n; ++i) r *= i;
    return r;
}

long long gcd(long long a, long long b) {
    while (b != 0) {
        long long t = a % b;
        a = b;
        b = t;
    }
    return a;
}

long long fib_iter(long long n) {
    long long a = 0, b = 1;
    for (long long i = 0; i < n; ++i) {
        long long t = a + b;
        a = b;
        b = t;
    }
    return a;
}

long long is_prime(long long n) {
    if (n < 2) return 0;
    for (long long d = 2; d * d <= n; ++d) {
        if (n % d == 0) return 0;
    }
    return 1;
}

long long int_pow(long long base, long long exp) {
    long long r = 1;
    for (long long i = 0; i < exp; ++i) r *= base;
    return r;
}

long long collatz_len(long long n) {
    long long steps = 0;
    while (n != 1) {
        if (n % 2 == 0) n /= 2;
        else n = 3 * n + 1;
        ++steps;
    }
    return steps;
}

long long digit_sum(long long n) {
    if (n < 0) n = -n;
    long long s = 0;
    while (n > 0) {
        s += n % 10;
        n /= 10;
    }
    return s;
}

long long count_primes_below(long long n) {
    long long count = 0;
    for (long long k = 2; k < n; ++k) {
        if (is_prime(k) == 1) ++count;
    }
    return count;
}

long long power_mod(long long base, long long exp, long long m) {
    long long r = 1;
    base %= m;
    while (exp > 0) {
        if (exp % 2 == 1) r = (r * base) % m;
        exp /= 2;
        base = (base * base) % m;
    }
    return r;
}

long long ackermann(long long m, long long n) {
    if (m == 0) return n + 1;
    if (n == 0) return ackermann(m - 1, 1);
    return ackermann(m - 1, ackermann(m, n - 1));
}

int main() {
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
    std::cout << "ok" << std::endl;
    return 0;
}

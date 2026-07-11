# Cross-language scalar function suite (Python). Same 16 hand-written scalar
# algorithms as ../lullaby/scalar.lby, using explicit loops/recursion (no
# math.factorial / math.gcd / three-arg pow / sympy). See ../SPEC.md.
# Benchmark inputs are non-negative, so Python's floored //, % match the
# reference's truncating remainder.


def add(a, b):
    return a + b


def max2(a, b):
    return a if a > b else b


def abs_val(n):
    if n < 0:
        return -n
    return n


def is_even(n):
    return 1 if n % 2 == 0 else 0


def clamp(x, lo, hi):
    if x < lo:
        return lo
    if x > hi:
        return hi
    return x


def sign(n):
    if n < 0:
        return -1
    if n > 0:
        return 1
    return 0


def factorial(n):
    r = 1
    i = 2
    while i <= n:
        r *= i
        i += 1
    return r


def gcd(a, b):
    while b != 0:
        a, b = b, a % b
    return a


def fib_iter(n):
    a, b = 0, 1
    i = 0
    while i < n:
        a, b = b, a + b
        i += 1
    return a


def is_prime(n):
    if n < 2:
        return 0
    d = 2
    while d * d <= n:
        if n % d == 0:
            return 0
        d += 1
    return 1


def int_pow(base, exp):
    r = 1
    i = 0
    while i < exp:
        r *= base
        i += 1
    return r


def collatz_len(n):
    steps = 0
    while n != 1:
        if n % 2 == 0:
            n //= 2
        else:
            n = 3 * n + 1
        steps += 1
    return steps


def digit_sum(n):
    if n < 0:
        n = -n
    s = 0
    while n > 0:
        s += n % 10
        n //= 10
    return s


def count_primes_below(n):
    count = 0
    k = 2
    while k < n:
        if is_prime(k) == 1:
            count += 1
        k += 1
    return count


def power_mod(base, exp, m):
    r = 1
    base %= m
    while exp > 0:
        if exp % 2 == 1:
            r = (r * base) % m
        exp //= 2
        base = (base * base) % m
    return r


def ackermann(m, n):
    if m == 0:
        return n + 1
    if n == 0:
        return ackermann(m - 1, 1)
    return ackermann(m - 1, ackermann(m, n - 1))


if __name__ == "__main__":
    assert add(2, 3) == 5
    assert max2(3, 7) == 7
    assert max2(5, 5) == 5
    assert abs_val(-4) == 4
    assert abs_val(0) == 0
    assert is_even(4) == 1
    assert is_even(7) == 0
    assert clamp(5, 0, 3) == 3
    assert clamp(-1, 0, 3) == 0
    assert clamp(2, 0, 3) == 2
    assert sign(-9) == -1
    assert sign(0) == 0
    assert sign(9) == 1
    assert factorial(5) == 120
    assert factorial(0) == 1
    assert gcd(48, 18) == 6
    assert gcd(7, 0) == 7
    assert fib_iter(10) == 55
    assert fib_iter(0) == 0
    assert is_prime(1) == 0
    assert is_prime(2) == 1
    assert is_prime(97) == 1
    assert is_prime(100) == 0
    assert int_pow(2, 10) == 1024
    assert int_pow(5, 0) == 1
    assert collatz_len(1) == 0
    assert collatz_len(6) == 8
    assert digit_sum(1234) == 10
    assert digit_sum(-90) == 9
    assert count_primes_below(100) == 25
    assert power_mod(7, 256, 13) == 9
    assert power_mod(2, 10, 1000) == 24
    assert ackermann(2, 3) == 9
    assert ackermann(3, 3) == 61
    print("ok")

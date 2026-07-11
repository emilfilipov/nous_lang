# Cross-language number theory suite (Python). Classic integer number theory.


def gcd(a, b):
    while b != 0:
        a, b = b, a % b
    return a


def int_pow(base, exp):
    r = 1
    for _ in range(exp):
        r = r * base
    return r


def num_digits(n):
    if n == 0:
        return 1
    c = 0
    while n > 0:
        c += 1
        n //= 10
    return c


def lcm(a, b):
    return a // gcd(a, b) * b


def divisor_count(n):
    count = 0
    d = 1
    while d * d <= n:
        if n % d == 0:
            count += 1
            if d != n // d:
                count += 1
        d += 1
    return count


def divisor_sum(n):
    total = 0
    d = 1
    while d * d <= n:
        if n % d == 0:
            total += d
            other = n // d
            if other != d:
                total += other
        d += 1
    return total


def is_perfect(n):
    return 1 if divisor_sum(n) - n == n else 0


def euler_totient(n):
    result = n
    p = 2
    while p * p <= n:
        if n % p == 0:
            while n % p == 0:
                n //= p
            result -= result // p
        p += 1
    if n > 1:
        result -= result // n
    return result


def count_coprime_below(n):
    count = 0
    for k in range(1, n):
        if gcd(k, n) == 1:
            count += 1
    return count


def digital_root(n):
    while n >= 10:
        s = 0
        while n > 0:
            s += n % 10
            n //= 10
        n = s
    return n


def is_armstrong(n):
    d = num_digits(n)
    total = 0
    m = n
    while m > 0:
        total += int_pow(m % 10, d)
        m //= 10
    return 1 if total == n else 0


def reverse_digits(n):
    r = 0
    while n > 0:
        r = r * 10 + n % 10
        n //= 10
    return r


def is_palindrome_number(n):
    return 1 if reverse_digits(n) == n else 0


def sum_of_squares_digits(n):
    total = 0
    while n > 0:
        d = n % 10
        total += d * d
        n //= 10
    return total


def is_happy(n):
    steps = 0
    while n != 1 and steps < 1000:
        n = sum_of_squares_digits(n)
        steps += 1
    return 1 if n == 1 else 0


def to_base_digit_sum(n, b):
    total = 0
    while n > 0:
        total += n % b
        n //= b
    return total


def count_trailing_zeros_factorial(n):
    count = 0
    power = 5
    while power <= n:
        count += n // power
        power *= 5
    return count


def gcd_of_range(lo, hi):
    g = lo
    for k in range(lo, hi + 1):
        g = gcd(g, k)
    return g


def main():
    print("lcm(12,18)=" + str(lcm(12, 18)))
    print("divisor_count(36)=" + str(divisor_count(36)))
    print("divisor_sum(36)=" + str(divisor_sum(36)))
    print("is_perfect(28)=" + str(is_perfect(28)))
    print("euler_totient(36)=" + str(euler_totient(36)))
    print("count_coprime_below(36)=" + str(count_coprime_below(36)))
    print("digital_root(9875)=" + str(digital_root(9875)))
    print("is_armstrong(153)=" + str(is_armstrong(153)))
    print("reverse_digits(1234)=" + str(reverse_digits(1234)))
    print("is_palindrome_number(1221)=" + str(is_palindrome_number(1221)))
    print("sum_of_squares_digits(123)=" + str(sum_of_squares_digits(123)))
    print("is_happy(19)=" + str(is_happy(19)))
    print("to_base_digit_sum(255,16)=" + str(to_base_digit_sum(255, 16)))
    print("count_trailing_zeros_factorial(100)=" + str(count_trailing_zeros_factorial(100)))
    print("gcd_of_range(12,24)=" + str(gcd_of_range(12, 24)))


if __name__ == "__main__":
    main()

// Cross-language number theory suite (C++). Classic integer number theory.
#include <cstdint>
#include <iostream>

int64_t gcd(int64_t a, int64_t b) {
    while (b != 0) {
        int64_t t = a % b;
        a = b;
        b = t;
    }
    return a;
}

int64_t int_pow(int64_t base, int64_t exp) {
    int64_t r = 1;
    for (int64_t i = 1; i <= exp; i++) {
        r = r * base;
    }
    return r;
}

int64_t num_digits(int64_t n) {
    if (n == 0) return 1;
    int64_t c = 0;
    while (n > 0) {
        c++;
        n = n / 10;
    }
    return c;
}

int64_t lcm(int64_t a, int64_t b) {
    return a / gcd(a, b) * b;
}

int64_t divisor_count(int64_t n) {
    int64_t count = 0;
    for (int64_t d = 1; d * d <= n; d++) {
        if (n % d == 0) {
            count++;
            if (d != n / d) count++;
        }
    }
    return count;
}

int64_t divisor_sum(int64_t n) {
    int64_t sum = 0;
    for (int64_t d = 1; d * d <= n; d++) {
        if (n % d == 0) {
            sum += d;
            int64_t other = n / d;
            if (other != d) sum += other;
        }
    }
    return sum;
}

int64_t is_perfect(int64_t n) {
    return divisor_sum(n) - n == n ? 1 : 0;
}

int64_t euler_totient(int64_t n) {
    int64_t result = n;
    for (int64_t p = 2; p * p <= n; p++) {
        if (n % p == 0) {
            while (n % p == 0) n = n / p;
            result = result - result / p;
        }
    }
    if (n > 1) result = result - result / n;
    return result;
}

int64_t count_coprime_below(int64_t n) {
    int64_t count = 0;
    for (int64_t k = 1; k <= n - 1; k++) {
        if (gcd(k, n) == 1) count++;
    }
    return count;
}

int64_t digital_root(int64_t n) {
    while (n >= 10) {
        int64_t s = 0;
        while (n > 0) {
            s += n % 10;
            n = n / 10;
        }
        n = s;
    }
    return n;
}

int64_t is_armstrong(int64_t n) {
    int64_t d = num_digits(n);
    int64_t sum = 0;
    int64_t m = n;
    while (m > 0) {
        sum += int_pow(m % 10, d);
        m = m / 10;
    }
    return sum == n ? 1 : 0;
}

int64_t reverse_digits(int64_t n) {
    int64_t r = 0;
    while (n > 0) {
        r = r * 10 + n % 10;
        n = n / 10;
    }
    return r;
}

int64_t is_palindrome_number(int64_t n) {
    return reverse_digits(n) == n ? 1 : 0;
}

int64_t sum_of_squares_digits(int64_t n) {
    int64_t sum = 0;
    while (n > 0) {
        int64_t d = n % 10;
        sum += d * d;
        n = n / 10;
    }
    return sum;
}

int64_t is_happy(int64_t n) {
    int64_t steps = 0;
    while (n != 1 && steps < 1000) {
        n = sum_of_squares_digits(n);
        steps++;
    }
    return n == 1 ? 1 : 0;
}

int64_t to_base_digit_sum(int64_t n, int64_t b) {
    int64_t sum = 0;
    while (n > 0) {
        sum += n % b;
        n = n / b;
    }
    return sum;
}

int64_t count_trailing_zeros_factorial(int64_t n) {
    int64_t count = 0;
    for (int64_t power = 5; power <= n; power = power * 5) {
        count += n / power;
    }
    return count;
}

int64_t gcd_of_range(int64_t lo, int64_t hi) {
    int64_t g = lo;
    for (int64_t k = lo; k <= hi; k++) {
        g = gcd(g, k);
    }
    return g;
}

int main() {
    std::cout << "lcm(12,18)=" << lcm(12, 18) << "\n";
    std::cout << "divisor_count(36)=" << divisor_count(36) << "\n";
    std::cout << "divisor_sum(36)=" << divisor_sum(36) << "\n";
    std::cout << "is_perfect(28)=" << is_perfect(28) << "\n";
    std::cout << "euler_totient(36)=" << euler_totient(36) << "\n";
    std::cout << "count_coprime_below(36)=" << count_coprime_below(36) << "\n";
    std::cout << "digital_root(9875)=" << digital_root(9875) << "\n";
    std::cout << "is_armstrong(153)=" << is_armstrong(153) << "\n";
    std::cout << "reverse_digits(1234)=" << reverse_digits(1234) << "\n";
    std::cout << "is_palindrome_number(1221)=" << is_palindrome_number(1221) << "\n";
    std::cout << "sum_of_squares_digits(123)=" << sum_of_squares_digits(123) << "\n";
    std::cout << "is_happy(19)=" << is_happy(19) << "\n";
    std::cout << "to_base_digit_sum(255,16)=" << to_base_digit_sum(255, 16) << "\n";
    std::cout << "count_trailing_zeros_factorial(100)=" << count_trailing_zeros_factorial(100) << "\n";
    std::cout << "gcd_of_range(12,24)=" << gcd_of_range(12, 24) << "\n";
    return 0;
}

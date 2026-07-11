/* Cross-language bitwise suite (C). Bit manipulation over int64 done with
   ARITHMETIC only: no bitwise operators (^ & | << >>) anywhere, to stay
   algorithm-identical to the Lullaby port, which models bit operations with arithmetic.
   Bits are read with % 2 and shifted with / 2 (or * a power of two). */
#include <stdio.h>
#include <stdint.h>

int64_t pow2(int64_t n) {
    int64_t p = 1;
    for (int64_t i = 1; i <= n; i++) p = p * 2;
    return p;
}

int64_t count_set_bits(int64_t x) {
    int64_t count = 0;
    while (x > 0) {
        count += x % 2;
        x /= 2;
    }
    return count;
}

int64_t is_power_of_two(int64_t x) {
    if (x <= 0) return 0;
    return count_set_bits(x) == 1 ? 1 : 0;
}

int64_t highest_bit_pos(int64_t x) {
    int64_t pos = -1;
    while (x > 0) {
        pos += 1;
        x /= 2;
    }
    return pos;
}

int64_t lowest_bit_pos(int64_t x) {
    if (x == 0) return -1;
    int64_t pos = 0;
    while (x % 2 == 0) {
        pos += 1;
        x /= 2;
    }
    return pos;
}

int64_t bit_at(int64_t x, int64_t i) {
    return (x / pow2(i)) % 2;
}

int64_t extract_bits(int64_t x, int64_t start, int64_t count) {
    return (x / pow2(start)) % pow2(count);
}

int64_t set_bit(int64_t x, int64_t i) {
    if (bit_at(x, i) == 1) return x;
    return x + pow2(i);
}

int64_t clear_bit(int64_t x, int64_t i) {
    if (bit_at(x, i) == 0) return x;
    return x - pow2(i);
}

int64_t toggle_bit(int64_t x, int64_t i) {
    if (bit_at(x, i) == 1) return x - pow2(i);
    return x + pow2(i);
}

int64_t popcount_range(int64_t lo, int64_t hi) {
    int64_t total = 0;
    for (int64_t v = lo; v <= hi; v++) total += count_set_bits(v);
    return total;
}

int64_t reverse_bits_n(int64_t x, int64_t n) {
    int64_t result = 0;
    for (int64_t i = 0; i < n; i++) {
        result = result * 2 + x % 2;
        x /= 2;
    }
    return result;
}

int64_t hamming_distance_bits(int64_t a, int64_t b) {
    int64_t dist = 0;
    while (a > 0 || b > 0) {
        if (a % 2 != b % 2) dist += 1;
        a /= 2;
        b /= 2;
    }
    return dist;
}

int64_t next_power_of_two(int64_t x) {
    int64_t p = 1;
    while (p < x) p = p * 2;
    return p;
}

int64_t count_leading_zeros_64(int64_t x) {
    if (x == 0) return 64;
    int64_t count = 64;
    while (x > 0) {
        count -= 1;
        x /= 2;
    }
    return count;
}

int64_t parity_bit(int64_t x) {
    return count_set_bits(x) % 2;
}

int64_t is_bit_palindrome(int64_t x, int64_t n) {
    return reverse_bits_n(x, n) == extract_bits(x, 0, n) ? 1 : 0;
}

int64_t rotate_left_n(int64_t x, int64_t n, int64_t bits) {
    int64_t width = pow2(bits);
    x = x % width;
    n = n % bits;
    int64_t hi = (x * pow2(n)) % width;
    int64_t lo = x / pow2(bits - n);
    return hi + lo;
}

int64_t merge_bits(int64_t a, int64_t b, int64_t mask_count) {
    int64_t m = pow2(mask_count);
    return a - a % m + b % m;
}

int main(void) {
    printf("count_set_bits=%lld\n", (long long)count_set_bits(181));
    printf("is_power_of_two=%lld\n", (long long)is_power_of_two(64));
    printf("highest_bit_pos=%lld\n", (long long)highest_bit_pos(181));
    printf("lowest_bit_pos=%lld\n", (long long)lowest_bit_pos(180));
    printf("bit_at=%lld\n", (long long)bit_at(181, 2));
    printf("set_bit=%lld\n", (long long)set_bit(181, 1));
    printf("clear_bit=%lld\n", (long long)clear_bit(181, 0));
    printf("toggle_bit=%lld\n", (long long)toggle_bit(181, 3));
    printf("popcount_range=%lld\n", (long long)popcount_range(0, 7));
    printf("reverse_bits_n=%lld\n", (long long)reverse_bits_n(13, 4));
    printf("hamming_distance_bits=%lld\n", (long long)hamming_distance_bits(181, 90));
    printf("next_power_of_two=%lld\n", (long long)next_power_of_two(100));
    printf("count_leading_zeros_64=%lld\n", (long long)count_leading_zeros_64(1));
    printf("parity_bit=%lld\n", (long long)parity_bit(181));
    printf("is_bit_palindrome=%lld\n", (long long)is_bit_palindrome(9, 4));
    printf("rotate_left_n=%lld\n", (long long)rotate_left_n(1, 1, 4));
    printf("extract_bits=%lld\n", (long long)extract_bits(181, 2, 3));
    printf("merge_bits=%lld\n", (long long)merge_bits(240, 15, 4));
    return 0;
}

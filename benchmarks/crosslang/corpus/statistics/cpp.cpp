// Cross-language statistics suite (C++). Integer statistics over an i64 array
// and a length: sums, spread, order statistics, and a Kadane max-subarray scan.
#include <cstdint>
#include <cstdlib>
#include <iostream>

std::int64_t total(const std::int64_t *a, std::int64_t n) {
    std::int64_t sum = 0;
    for (std::int64_t i = 0; i < n; i++) sum += a[i];
    return sum;
}

std::int64_t mean_floor(const std::int64_t *a, std::int64_t n) {
    return total(a, n) / n;
}

std::int64_t variance_scaled(const std::int64_t *a, std::int64_t n) {
    std::int64_t mean = mean_floor(a, n), sum = 0;
    for (std::int64_t i = 0; i < n; i++) {
        std::int64_t d = a[i] - mean;
        sum += d * d;
    }
    return sum;
}

std::int64_t min_val(const std::int64_t *a, std::int64_t n) {
    std::int64_t m = a[0];
    for (std::int64_t i = 1; i < n; i++) if (a[i] < m) m = a[i];
    return m;
}

std::int64_t max_val(const std::int64_t *a, std::int64_t n) {
    std::int64_t m = a[0];
    for (std::int64_t i = 1; i < n; i++) if (a[i] > m) m = a[i];
    return m;
}

std::int64_t range_span(const std::int64_t *a, std::int64_t n) {
    return max_val(a, n) - min_val(a, n);
}

std::int64_t median_sorted(const std::int64_t *a, std::int64_t n) {
    return a[(n - 1) / 2];
}

std::int64_t mode_count_max(const std::int64_t *a, std::int64_t n) {
    if (n == 0) return 0;
    std::int64_t best = 1, run = 1;
    for (std::int64_t i = 1; i < n; i++) {
        run = (a[i] == a[i - 1]) ? run + 1 : 1;
        if (run > best) best = run;
    }
    return best;
}

std::int64_t count_above_mean(const std::int64_t *a, std::int64_t n) {
    std::int64_t mean = mean_floor(a, n), count = 0;
    for (std::int64_t i = 0; i < n; i++) if (a[i] > mean) count++;
    return count;
}

std::int64_t sum_abs_dev(const std::int64_t *a, std::int64_t n) {
    std::int64_t mean = mean_floor(a, n), sum = 0;
    for (std::int64_t i = 0; i < n; i++) sum += std::llabs(a[i] - mean);
    return sum;
}

std::int64_t product_mod(const std::int64_t *a, std::int64_t n, std::int64_t m) {
    std::int64_t prod = 1;
    for (std::int64_t i = 0; i < n; i++) prod = (prod * a[i]) % m;
    return prod;
}

std::int64_t running_max_sum(const std::int64_t *a, std::int64_t n) {
    std::int64_t best = a[0], cur = a[0];
    for (std::int64_t i = 1; i < n; i++) {
        cur = (cur + a[i] > a[i]) ? cur + a[i] : a[i];
        if (cur > best) best = cur;
    }
    return best;
}

std::int64_t zscore_sign_count(const std::int64_t *a, std::int64_t n) {
    std::int64_t mean = mean_floor(a, n), count = 0;
    for (std::int64_t i = 0; i < n; i++) if (a[i] > mean) count++;
    return count;
}

std::int64_t weighted_sum(const std::int64_t *a, const std::int64_t *w, std::int64_t n) {
    std::int64_t sum = 0;
    for (std::int64_t i = 0; i < n; i++) sum += a[i] * w[i];
    return sum;
}

std::int64_t cumulative_max_last(const std::int64_t *a, std::int64_t n) {
    std::int64_t m = a[0];
    for (std::int64_t i = 1; i < n; i++) if (a[i] > m) m = a[i];
    return m;
}

int main() {
    std::int64_t d[6] = { 3, -7, 4, 8, -2, 5 };
    std::int64_t s[6] = { 2, 2, 2, 5, 7, 8 };
    std::int64_t w[6] = { 1, 2, 1, 3, 1, 2 };
    std::cout << "total=" << total(d, 6) << "\n";
    std::cout << "mean_floor=" << mean_floor(d, 6) << "\n";
    std::cout << "variance_scaled=" << variance_scaled(d, 6) << "\n";
    std::cout << "min_val=" << min_val(d, 6) << "\n";
    std::cout << "max_val=" << max_val(d, 6) << "\n";
    std::cout << "range_span=" << range_span(d, 6) << "\n";
    std::cout << "median_sorted=" << median_sorted(s, 6) << "\n";
    std::cout << "mode_count_max=" << mode_count_max(s, 6) << "\n";
    std::cout << "count_above_mean=" << count_above_mean(d, 6) << "\n";
    std::cout << "sum_abs_dev=" << sum_abs_dev(d, 6) << "\n";
    std::cout << "product_mod=" << product_mod(s, 6, 1000) << "\n";
    std::cout << "running_max_sum=" << running_max_sum(d, 6) << "\n";
    std::cout << "zscore_sign_count=" << zscore_sign_count(d, 6) << "\n";
    std::cout << "weighted_sum=" << weighted_sum(d, w, 6) << "\n";
    std::cout << "cumulative_max_last=" << cumulative_max_last(d, 6) << "\n";
    return 0;
}

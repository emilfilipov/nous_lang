/* Cross-language statistics suite (C). Integer statistics over an i64 array and
   a length: sums, spread, order statistics, and a Kadane max-subarray scan. */
#include <stdio.h>
#include <stdint.h>

int64_t total(const int64_t *a, int64_t n) {
    int64_t sum = 0;
    for (int64_t i = 0; i < n; i++) sum += a[i];
    return sum;
}

int64_t mean_floor(const int64_t *a, int64_t n) {
    return total(a, n) / n;
}

int64_t variance_scaled(const int64_t *a, int64_t n) {
    int64_t mean = mean_floor(a, n), sum = 0;
    for (int64_t i = 0; i < n; i++) {
        int64_t d = a[i] - mean;
        sum += d * d;
    }
    return sum;
}

int64_t min_val(const int64_t *a, int64_t n) {
    int64_t m = a[0];
    for (int64_t i = 1; i < n; i++) if (a[i] < m) m = a[i];
    return m;
}

int64_t max_val(const int64_t *a, int64_t n) {
    int64_t m = a[0];
    for (int64_t i = 1; i < n; i++) if (a[i] > m) m = a[i];
    return m;
}

int64_t range_span(const int64_t *a, int64_t n) {
    return max_val(a, n) - min_val(a, n);
}

int64_t median_sorted(const int64_t *a, int64_t n) {
    return a[(n - 1) / 2];
}

int64_t mode_count_max(const int64_t *a, int64_t n) {
    if (n == 0) return 0;
    int64_t best = 1, run = 1;
    for (int64_t i = 1; i < n; i++) {
        run = (a[i] == a[i - 1]) ? run + 1 : 1;
        if (run > best) best = run;
    }
    return best;
}

int64_t count_above_mean(const int64_t *a, int64_t n) {
    int64_t mean = mean_floor(a, n), count = 0;
    for (int64_t i = 0; i < n; i++) if (a[i] > mean) count++;
    return count;
}

int64_t sum_abs_dev(const int64_t *a, int64_t n) {
    int64_t mean = mean_floor(a, n), sum = 0;
    for (int64_t i = 0; i < n; i++) {
        int64_t d = a[i] - mean;
        sum += d < 0 ? -d : d;
    }
    return sum;
}

int64_t product_mod(const int64_t *a, int64_t n, int64_t m) {
    int64_t prod = 1;
    for (int64_t i = 0; i < n; i++) prod = (prod * a[i]) % m;
    return prod;
}

int64_t running_max_sum(const int64_t *a, int64_t n) {
    int64_t best = a[0], cur = a[0];
    for (int64_t i = 1; i < n; i++) {
        cur = (cur + a[i] > a[i]) ? cur + a[i] : a[i];
        if (cur > best) best = cur;
    }
    return best;
}

int64_t zscore_sign_count(const int64_t *a, int64_t n) {
    int64_t mean = mean_floor(a, n), count = 0;
    for (int64_t i = 0; i < n; i++) if (a[i] > mean) count++;
    return count;
}

int64_t weighted_sum(const int64_t *a, const int64_t *w, int64_t n) {
    int64_t sum = 0;
    for (int64_t i = 0; i < n; i++) sum += a[i] * w[i];
    return sum;
}

int64_t cumulative_max_last(const int64_t *a, int64_t n) {
    int64_t m = a[0];
    for (int64_t i = 1; i < n; i++) if (a[i] > m) m = a[i];
    return m;
}

int main(void) {
    int64_t d[6] = { 3, -7, 4, 8, -2, 5 };
    int64_t s[6] = { 2, 2, 2, 5, 7, 8 };
    int64_t w[6] = { 1, 2, 1, 3, 1, 2 };
    printf("total=%lld\n", (long long)total(d, 6));
    printf("mean_floor=%lld\n", (long long)mean_floor(d, 6));
    printf("variance_scaled=%lld\n", (long long)variance_scaled(d, 6));
    printf("min_val=%lld\n", (long long)min_val(d, 6));
    printf("max_val=%lld\n", (long long)max_val(d, 6));
    printf("range_span=%lld\n", (long long)range_span(d, 6));
    printf("median_sorted=%lld\n", (long long)median_sorted(s, 6));
    printf("mode_count_max=%lld\n", (long long)mode_count_max(s, 6));
    printf("count_above_mean=%lld\n", (long long)count_above_mean(d, 6));
    printf("sum_abs_dev=%lld\n", (long long)sum_abs_dev(d, 6));
    printf("product_mod=%lld\n", (long long)product_mod(s, 6, 1000));
    printf("running_max_sum=%lld\n", (long long)running_max_sum(d, 6));
    printf("zscore_sign_count=%lld\n", (long long)zscore_sign_count(d, 6));
    printf("weighted_sum=%lld\n", (long long)weighted_sum(d, w, 6));
    printf("cumulative_max_last=%lld\n", (long long)cumulative_max_last(d, 6));
    return 0;
}

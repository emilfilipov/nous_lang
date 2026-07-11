"""Cross-language statistics suite (Python). Integer statistics over a list and
a length: sums, spread, order statistics, and a Kadane max-subarray scan."""


def total(a: list, n: int) -> int:
    s = 0
    for i in range(n):
        s += a[i]
    return s


def mean_floor(a: list, n: int) -> int:
    return total(a, n) // n


def variance_scaled(a: list, n: int) -> int:
    mean = mean_floor(a, n)
    s = 0
    for i in range(n):
        d = a[i] - mean
        s += d * d
    return s


def min_val(a: list, n: int) -> int:
    m = a[0]
    for i in range(1, n):
        if a[i] < m:
            m = a[i]
    return m


def max_val(a: list, n: int) -> int:
    m = a[0]
    for i in range(1, n):
        if a[i] > m:
            m = a[i]
    return m


def range_span(a: list, n: int) -> int:
    return max_val(a, n) - min_val(a, n)


def median_sorted(a: list, n: int) -> int:
    return a[(n - 1) // 2]


def mode_count_max(a: list, n: int) -> int:
    if n == 0:
        return 0
    best = 1
    run = 1
    for i in range(1, n):
        run = run + 1 if a[i] == a[i - 1] else 1
        if run > best:
            best = run
    return best


def count_above_mean(a: list, n: int) -> int:
    mean = mean_floor(a, n)
    count = 0
    for i in range(n):
        if a[i] > mean:
            count += 1
    return count


def sum_abs_dev(a: list, n: int) -> int:
    mean = mean_floor(a, n)
    s = 0
    for i in range(n):
        s += abs(a[i] - mean)
    return s


def product_mod(a: list, n: int, m: int) -> int:
    prod = 1
    for i in range(n):
        prod = (prod * a[i]) % m
    return prod


def running_max_sum(a: list, n: int) -> int:
    best = a[0]
    cur = a[0]
    for i in range(1, n):
        cur = cur + a[i] if cur + a[i] > a[i] else a[i]
        if cur > best:
            best = cur
    return best


def zscore_sign_count(a: list, n: int) -> int:
    mean = mean_floor(a, n)
    count = 0
    for i in range(n):
        if a[i] > mean:
            count += 1
    return count


def weighted_sum(a: list, w: list, n: int) -> int:
    s = 0
    for i in range(n):
        s += a[i] * w[i]
    return s


def cumulative_max_last(a: list, n: int) -> int:
    m = a[0]
    for i in range(1, n):
        if a[i] > m:
            m = a[i]
    return m


def main() -> None:
    d = [3, -7, 4, 8, -2, 5]
    s = [2, 2, 2, 5, 7, 8]
    w = [1, 2, 1, 3, 1, 2]
    print("total=" + str(total(d, 6)))
    print("mean_floor=" + str(mean_floor(d, 6)))
    print("variance_scaled=" + str(variance_scaled(d, 6)))
    print("min_val=" + str(min_val(d, 6)))
    print("max_val=" + str(max_val(d, 6)))
    print("range_span=" + str(range_span(d, 6)))
    print("median_sorted=" + str(median_sorted(s, 6)))
    print("mode_count_max=" + str(mode_count_max(s, 6)))
    print("count_above_mean=" + str(count_above_mean(d, 6)))
    print("sum_abs_dev=" + str(sum_abs_dev(d, 6)))
    print("product_mod=" + str(product_mod(s, 6, 1000)))
    print("running_max_sum=" + str(running_max_sum(d, 6)))
    print("zscore_sign_count=" + str(zscore_sign_count(d, 6)))
    print("weighted_sum=" + str(weighted_sum(d, w, 6)))
    print("cumulative_max_last=" + str(cumulative_max_last(d, 6)))


if __name__ == "__main__":
    main()

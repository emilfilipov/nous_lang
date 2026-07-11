// Cross-language statistics suite (Rust). Integer statistics over an i64 slice
// and a length: sums, spread, order statistics, and a Kadane max-subarray scan.

fn total(a: &[i64], n: i64) -> i64 {
    let mut sum = 0;
    for i in 0..n as usize { sum += a[i]; }
    sum
}

fn mean_floor(a: &[i64], n: i64) -> i64 {
    total(a, n) / n
}

fn variance_scaled(a: &[i64], n: i64) -> i64 {
    let mean = mean_floor(a, n);
    let mut sum = 0;
    for i in 0..n as usize {
        let d = a[i] - mean;
        sum += d * d;
    }
    sum
}

fn min_val(a: &[i64], n: i64) -> i64 {
    let mut m = a[0];
    for i in 1..n as usize { if a[i] < m { m = a[i]; } }
    m
}

fn max_val(a: &[i64], n: i64) -> i64 {
    let mut m = a[0];
    for i in 1..n as usize { if a[i] > m { m = a[i]; } }
    m
}

fn range_span(a: &[i64], n: i64) -> i64 {
    max_val(a, n) - min_val(a, n)
}

fn median_sorted(a: &[i64], n: i64) -> i64 {
    a[((n - 1) / 2) as usize]
}

fn mode_count_max(a: &[i64], n: i64) -> i64 {
    if n == 0 { return 0; }
    let mut best = 1;
    let mut run = 1;
    for i in 1..n as usize {
        run = if a[i] == a[i - 1] { run + 1 } else { 1 };
        if run > best { best = run; }
    }
    best
}

fn count_above_mean(a: &[i64], n: i64) -> i64 {
    let mean = mean_floor(a, n);
    let mut count = 0;
    for i in 0..n as usize { if a[i] > mean { count += 1; } }
    count
}

fn sum_abs_dev(a: &[i64], n: i64) -> i64 {
    let mean = mean_floor(a, n);
    let mut sum = 0;
    for i in 0..n as usize { sum += (a[i] - mean).abs(); }
    sum
}

fn product_mod(a: &[i64], n: i64, m: i64) -> i64 {
    let mut prod = 1;
    for i in 0..n as usize { prod = (prod * a[i]) % m; }
    prod
}

fn running_max_sum(a: &[i64], n: i64) -> i64 {
    let mut best = a[0];
    let mut cur = a[0];
    for i in 1..n as usize {
        cur = if cur + a[i] > a[i] { cur + a[i] } else { a[i] };
        if cur > best { best = cur; }
    }
    best
}

fn zscore_sign_count(a: &[i64], n: i64) -> i64 {
    let mean = mean_floor(a, n);
    let mut count = 0;
    for i in 0..n as usize { if a[i] > mean { count += 1; } }
    count
}

fn weighted_sum(a: &[i64], w: &[i64], n: i64) -> i64 {
    let mut sum = 0;
    for i in 0..n as usize { sum += a[i] * w[i]; }
    sum
}

fn cumulative_max_last(a: &[i64], n: i64) -> i64 {
    let mut m = a[0];
    for i in 1..n as usize { if a[i] > m { m = a[i]; } }
    m
}

fn main() {
    let d = [3i64, -7, 4, 8, -2, 5];
    let s = [2i64, 2, 2, 5, 7, 8];
    let w = [1i64, 2, 1, 3, 1, 2];
    println!("total={}", total(&d, 6));
    println!("mean_floor={}", mean_floor(&d, 6));
    println!("variance_scaled={}", variance_scaled(&d, 6));
    println!("min_val={}", min_val(&d, 6));
    println!("max_val={}", max_val(&d, 6));
    println!("range_span={}", range_span(&d, 6));
    println!("median_sorted={}", median_sorted(&s, 6));
    println!("mode_count_max={}", mode_count_max(&s, 6));
    println!("count_above_mean={}", count_above_mean(&d, 6));
    println!("sum_abs_dev={}", sum_abs_dev(&d, 6));
    println!("product_mod={}", product_mod(&s, 6, 1000));
    println!("running_max_sum={}", running_max_sum(&d, 6));
    println!("zscore_sign_count={}", zscore_sign_count(&d, 6));
    println!("weighted_sum={}", weighted_sum(&d, &w, 6));
    println!("cumulative_max_last={}", cumulative_max_last(&d, 6));
}

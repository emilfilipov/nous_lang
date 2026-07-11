fn is_prime(n: i64) -> i64 { if n < 2 { return 0; } let mut d = 2; while d*d <= n { if n%d == 0 { return 0; } d += 1; } 1 }
fn count_primes_below(n: i64) -> i64 { let mut c = 0; let mut k = 2; while k < n { c += is_prime(k); k += 1; } c }
fn main() { println!("{}", count_primes_below(300000)); }

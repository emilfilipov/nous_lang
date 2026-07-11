def is_prime(n):
    if n < 2: return 0
    d = 2
    while d*d <= n:
        if n % d == 0: return 0
        d += 1
    return 1
def count_primes_below(n):
    c = 0
    for k in range(2, n):
        c += is_prime(k)
    return c
if __name__ == "__main__":
    print(count_primes_below(300000))

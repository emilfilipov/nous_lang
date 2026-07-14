/* C reference for the gcd (Euclid) benchmark. N is supplied at compile time via
 * /DGCD_N=<count>; the loop accumulates gcd(i, 1071) for i in 1..N. Mirrors the
 * Lullaby `gcd` builtin's unsigned-magnitude Euclid so the two are comparable. */
#include <stdio.h>

static long long gcd_ll(long long a, long long b) {
    unsigned long long x = a < 0 ? -(unsigned long long)a : (unsigned long long)a;
    unsigned long long y = b < 0 ? -(unsigned long long)b : (unsigned long long)b;
    while (y) {
        unsigned long long r = x % y;
        x = y;
        y = r;
    }
    return (long long)x;
}

int main(void) {
    long long acc = 0;
    for (long long i = 1; i < GCD_N; i++) acc += gcd_ll(i, 1071);
    printf("%lld\n", acc);
    return 0;
}

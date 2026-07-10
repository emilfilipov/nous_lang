/* Recursive Fibonacci baseline for the Lullaby benchmark suite.
 * Mirrors benchmarks/fib.lby exactly (i64, naive two-way recursion) so the
 * per-call cost is directly comparable to `lullaby native`. N comes from argv
 * so the same binary serves every measured depth. Build: cl /O2 fib.c */
#include <stdio.h>
#include <stdlib.h>

static long long fib(long long n) {
    if (n < 2) return n;
    return fib(n - 1) + fib(n - 2);
}

int main(int argc, char **argv) {
    long long n = argc > 1 ? atoll(argv[1]) : 40;
    printf("%lld\n", fib(n));
    return 0;
}

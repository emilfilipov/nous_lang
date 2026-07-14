/* C reference for the CSV-integer parse+aggregate benchmark
 * (benchmarks/run_csvsum.ps1) - a small realistic data transform.
 *
 * Mirrors the Lullaby program: a fixed ASCII string of comma-separated decimal
 * integers is parsed field by field (a hand-rolled digit accumulator, exactly
 * `cur = cur*10 + digit`, flushing on any non-digit) and the parsed integers are
 * summed, once per repetition; the per-scan totals accumulate over `reps`
 * repetitions. Exercises more than one feature at once - string indexing,
 * per-character branching, multiply/add, and accumulation. The string literal
 * below is byte-for-byte the same data the .ps1 harness bakes into its generated
 * .lby. `reps` comes from argv. Build: cl /O2 csvsum.c */
#include <stdio.h>
#include <stdlib.h>

static const char *S =
    "12,345,6,7890,42,1,999,23,4567,8,90,123,4,56,7,808,11,222,3,44";

static long long parse_sum(void) {
    long long total = 0, cur = 0;
    for (const char *p = S; *p; p++) {
        int c = (unsigned char)*p;
        if (c >= 48 && c <= 57) {
            cur = cur * 10 + (c - 48);
        } else {
            total += cur;
            cur = 0;
        }
    }
    total += cur;
    return total;
}

int main(int argc, char **argv) {
    long long reps = argc > 1 ? atoll(argv[1]) : 1;
    long long acc = 0;
    for (long long r = 0; r < reps; r++) acc += parse_sum();
    printf("%lld\n", acc);
    return 0;
}

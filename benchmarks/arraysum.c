/* C reference for the array-scan benchmark (benchmarks/run_arraysum.ps1).
 *
 * Mirrors the Lullaby program exactly: a fixed 64-element i64 array is scanned
 * (summed) once per repetition, and the per-scan sums are accumulated over
 * `reps` repetitions. The array literal below is byte-for-byte the same data
 * the .ps1 harness bakes into its generated .lby, so the two are directly
 * comparable. `reps` comes from argv so one binary serves every measured count.
 * Build: cl /O2 arraysum.c */
#include <stdio.h>
#include <stdlib.h>

static const long long A[] = {
    3, 1, 4, 1, 5, 9, 2, 6, 5, 3, 5, 8, 9, 7, 9, 3,
    2, 3, 8, 4, 6, 2, 6, 4, 3, 3, 8, 3, 2, 7, 9, 5,
    0, 2, 8, 8, 4, 1, 9, 7, 1, 6, 9, 3, 9, 9, 3, 7,
    5, 1, 0, 5, 8, 2, 0, 9, 7, 4, 9, 4, 4, 5, 9, 2
};
static const int AN = (int)(sizeof(A) / sizeof(A[0]));

static long long scan(void) {
    long long s = 0;
    for (int i = 0; i < AN; i++) s += A[i];
    return s;
}

int main(int argc, char **argv) {
    long long reps = argc > 1 ? atoll(argv[1]) : 1;
    long long acc = 0;
    for (long long r = 0; r < reps; r++) acc += scan();
    printf("%lld\n", acc);
    return 0;
}

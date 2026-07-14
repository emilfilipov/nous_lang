/* C reference for the string char-scan benchmark (benchmarks/run_strscan.ps1).
 *
 * Mirrors the Lullaby program: a fixed ASCII string is scanned character by
 * character, summing each character's code, once per repetition; the per-scan
 * checksums accumulate over `reps` repetitions. The string is ASCII, so a byte
 * value equals the Unicode code point Lullaby's `char_code` returns, making the
 * two checksums identical. The string literal below is byte-for-byte the same
 * data the .ps1 harness bakes into its generated .lby. `reps` comes from argv.
 * Build: cl /O2 strscan.c */
#include <stdio.h>
#include <stdlib.h>

static const char *S =
    "the quick brown fox jumps over the lazy dog while 12345 sheep leap";

static long long checksum(void) {
    long long sum = 0;
    for (const char *p = S; *p; p++) sum += (unsigned char)*p;
    return sum;
}

int main(int argc, char **argv) {
    long long reps = argc > 1 ? atoll(argv[1]) : 1;
    long long acc = 0;
    for (long long r = 0; r < reps; r++) acc += checksum();
    printf("%lld\n", acc);
    return 0;
}

// Cross-language parsing suite (C). Real-world parsing over strings and i64 arrays
// mirroring ../lullaby.lby. Strings are `const char*`; `eval_rpn` takes an i64
// array plus a length and uses a stack. Invalid numeric input returns -1 (or 0 for
// the signed parser, whose full range includes -1). See ../SPEC.md.

#include <assert.h>
#include <stdio.h>
#include <string.h>

typedef long long i64;

i64 parse_uint(const char *s) {
    size_t n = strlen(s);
    if (n == 0) return -1;
    i64 val = 0;
    for (size_t i = 0; i < n; i++) {
        char c = s[i];
        if (c < '0' || c > '9') return -1;
        val = val * 10 + (c - '0');
    }
    return val;
}

i64 parse_int_signed(const char *s) {
    size_t n = strlen(s);
    if (n == 0) return 0;
    int neg = 0;
    size_t start = 0;
    if (s[0] == '-') { neg = 1; start = 1; }
    if (start == n) return 0;
    i64 val = 0;
    for (size_t i = start; i < n; i++) {
        char c = s[i];
        if (c < '0' || c > '9') return 0;
        val = val * 10 + (c - '0');
    }
    return neg ? -val : val;
}

i64 is_valid_int(const char *s) {
    size_t n = strlen(s);
    if (n == 0) return 0;
    size_t start = (s[0] == '-') ? 1 : 0;
    if (start == n) return 0;
    for (size_t i = start; i < n; i++)
        if (s[i] < '0' || s[i] > '9') return 0;
    return 1;
}

i64 count_fields(const char *s, const char *sep) {
    char target = sep[0];
    i64 count = 1;
    for (const char *p = s; *p; p++)
        if (*p == target) count++;
    return count;
}

i64 nth_field_len(const char *s, const char *sep, i64 nth) {
    char target = sep[0];
    i64 field = 0, cur = 0, result = -1;
    for (const char *p = s; *p; p++) {
        if (*p == target) {
            if (field == nth) result = cur;
            field++;
            cur = 0;
        } else {
            cur++;
        }
    }
    if (field == nth) result = cur;
    return result;
}

i64 count_lines(const char *s) {
    size_t n = strlen(s);
    if (n == 0) return 0;
    i64 count = 1;
    for (size_t i = 0; i < n; i++)
        if (s[i] == '\n') count++;
    return count;
}

i64 strip_leading_zeros_len(const char *s) {
    size_t n = strlen(s), i = 0;
    while (i < n && s[i] == '0') i++;
    return (i64)(n - i);
}

i64 eval_rpn(const i64 *tokens, i64 n) {
    if (n == 0) return 0;
    i64 stack[256];
    i64 sp = 0;
    for (i64 i = 0; i < n; i++) {
        i64 t = tokens[i];
        if (t >= 0) {
            stack[sp++] = t;
        } else {
            i64 a = stack[sp - 2], b = stack[sp - 1];
            sp -= 2;
            i64 op = -t, r = 0;
            if (op == 1) r = a + b;
            else if (op == 2) r = a - b;
            else if (op == 3) r = a * b;
            else r = a / b;
            stack[sp++] = r;
        }
    }
    return stack[0];
}

i64 count_digits_in(const char *s) {
    i64 count = 0;
    for (const char *p = s; *p; p++)
        if (*p >= '0' && *p <= '9') count++;
    return count;
}

i64 count_words(const char *s) {
    i64 count = 0;
    int in_word = 0;
    for (const char *p = s; *p; p++) {
        if (*p == ' ' || *p == '\t' || *p == '\n') {
            in_word = 0;
        } else if (!in_word) {
            in_word = 1;
            count++;
        }
    }
    return count;
}

i64 hex_to_int(const char *s) {
    size_t n = strlen(s);
    if (n == 0) return -1;
    i64 val = 0;
    for (size_t i = 0; i < n; i++) {
        char c = s[i];
        i64 d = -1;
        if (c >= '0' && c <= '9') d = c - '0';
        else if (c >= 'a' && c <= 'f') d = c - 'a' + 10;
        else if (c >= 'A' && c <= 'F') d = c - 'A' + 10;
        if (d < 0) return -1;
        val = val * 16 + d;
    }
    return val;
}

i64 bin_to_int(const char *s) {
    size_t n = strlen(s);
    if (n == 0) return -1;
    i64 val = 0;
    for (size_t i = 0; i < n; i++) {
        char c = s[i];
        if (c == '0') val = val * 2;
        else if (c == '1') val = val * 2 + 1;
        else return -1;
    }
    return val;
}

i64 roman_value(char c) {
    switch (c) {
        case 'I': return 1;
        case 'V': return 5;
        case 'X': return 10;
        case 'L': return 50;
        case 'C': return 100;
        case 'D': return 500;
        case 'M': return 1000;
        default: return 0;
    }
}

i64 roman_to_int(const char *s) {
    size_t n = strlen(s);
    i64 total = 0;
    for (size_t i = 0; i < n; i++) {
        i64 v = roman_value(s[i]);
        if (i + 1 < n && v < roman_value(s[i + 1])) total -= v;
        else total += v;
    }
    return total;
}

i64 char_class_count(const char *s) {
    i64 count = 0;
    for (const char *p = s; *p; p++)
        if ((*p >= 'A' && *p <= 'Z') || (*p >= 'a' && *p <= 'z')) count++;
    return count;
}

int main(void) {
    i64 rpn[] = {3, 4, -1, 5, -3};
    assert(parse_uint("01234") == 1234);
    assert(parse_int_signed("-42") == -42);
    assert(is_valid_int("-42") == 1);
    assert(count_fields("a,b,c,d", ",") == 4);
    assert(nth_field_len("a,bb,ccc", ",", 2) == 3);
    assert(count_lines("a\nb") == 2);
    assert(strip_leading_zeros_len("00042") == 2);
    assert(eval_rpn(rpn, 5) == 35);
    assert(count_digits_in("ab12cd34") == 4);
    assert(count_words("the quick brown fox") == 4);
    assert(hex_to_int("1a2f") == 6703);
    assert(bin_to_int("101101") == 45);
    assert(roman_to_int("MCMXCIV") == 1994);
    assert(char_class_count("abc123XYZ") == 6);
    printf("ok\n");
    return 0;
}

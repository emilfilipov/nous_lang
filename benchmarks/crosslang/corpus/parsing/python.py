# Cross-language parsing suite (Python). Real-world parsing over strings and int
# arrays mirroring ../lullaby.lby. `eval_rpn` takes a list plus a length. Invalid
# numeric input returns -1 (or 0 for the signed parser, whose full range includes
# -1). See ../SPEC.md.


def parse_uint(s):
    if not s:
        return -1
    val = 0
    for c in s:
        if not ("0" <= c <= "9"):
            return -1
        val = val * 10 + (ord(c) - 48)
    return val


def parse_int_signed(s):
    if not s:
        return 0
    neg = s[0] == "-"
    start = 1 if neg else 0
    if start == len(s):
        return 0
    val = 0
    for c in s[start:]:
        if not ("0" <= c <= "9"):
            return 0
        val = val * 10 + (ord(c) - 48)
    return -val if neg else val


def is_valid_int(s):
    if not s:
        return 0
    start = 1 if s[0] == "-" else 0
    if start == len(s):
        return 0
    return 1 if s[start:].isdigit() else 0


def count_fields(s, sep):
    return s.count(sep[0]) + 1


def nth_field_len(s, sep, nth):
    target = sep[0]
    field = 0
    cur = 0
    result = -1
    for c in s:
        if c == target:
            if field == nth:
                result = cur
            field += 1
            cur = 0
        else:
            cur += 1
    if field == nth:
        result = cur
    return result


def count_lines(s):
    if not s:
        return 0
    return s.count("\n") + 1


def strip_leading_zeros_len(s):
    i = 0
    while i < len(s) and s[i] == "0":
        i += 1
    return len(s) - i


def eval_rpn(tokens, n):
    if n == 0:
        return 0
    stack = []
    for t in tokens[:n]:
        if t >= 0:
            stack.append(t)
        else:
            b = stack.pop()
            a = stack.pop()
            op = -t
            if op == 1:
                r = a + b
            elif op == 2:
                r = a - b
            elif op == 3:
                r = a * b
            else:
                r = int(a / b)
            stack.append(r)
    return stack[0]


def count_digits_in(s):
    count = 0
    for c in s:
        if "0" <= c <= "9":
            count += 1
    return count


def count_words(s):
    return len(s.split())


def hex_to_int(s):
    if not s:
        return -1
    val = 0
    for c in s:
        o = ord(c)
        if 48 <= o <= 57:
            d = o - 48
        elif 97 <= o <= 102:
            d = o - 97 + 10
        elif 65 <= o <= 70:
            d = o - 65 + 10
        else:
            return -1
        val = val * 16 + d
    return val


def bin_to_int(s):
    if not s:
        return -1
    val = 0
    for c in s:
        if c == "0":
            val = val * 2
        elif c == "1":
            val = val * 2 + 1
        else:
            return -1
    return val


def roman_value(c):
    return {
        "I": 1,
        "V": 5,
        "X": 10,
        "L": 50,
        "C": 100,
        "D": 500,
        "M": 1000,
    }.get(c, 0)


def roman_to_int(s):
    total = 0
    n = len(s)
    for i in range(n):
        v = roman_value(s[i])
        if i + 1 < n and v < roman_value(s[i + 1]):
            total -= v
        else:
            total += v
    return total


def char_class_count(s):
    count = 0
    for c in s:
        if ("A" <= c <= "Z") or ("a" <= c <= "z"):
            count += 1
    return count


if __name__ == "__main__":
    rpn = [3, 4, -1, 5, -3]
    assert parse_uint("01234") == 1234
    assert parse_int_signed("-42") == -42
    assert is_valid_int("-42") == 1
    assert count_fields("a,b,c,d", ",") == 4
    assert nth_field_len("a,bb,ccc", ",", 2) == 3
    assert count_lines("a\nb") == 2
    assert strip_leading_zeros_len("00042") == 2
    assert eval_rpn(rpn, 5) == 35
    assert count_digits_in("ab12cd34") == 4
    assert count_words("the quick brown fox") == 4
    assert hex_to_int("1a2f") == 6703
    assert bin_to_int("101101") == 45
    assert roman_to_int("MCMXCIV") == 1994
    assert char_class_count("abc123XYZ") == 6
    print("ok")

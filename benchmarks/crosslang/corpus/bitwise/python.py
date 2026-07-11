"""Cross-language bitwise suite (Python). Bit manipulation over int done with
ARITHMETIC only: no bitwise operators (^ & | << >>) anywhere, to stay
algorithm-identical to the Lullaby port, which models bit operations with arithmetic. Bits are
read with % 2 and shifted with // 2 (or * a power of two)."""


def pow2(n: int) -> int:
    p = 1
    for _ in range(1, n + 1):
        p = p * 2
    return p


def count_set_bits(x: int) -> int:
    count = 0
    while x > 0:
        count += x % 2
        x //= 2
    return count


def is_power_of_two(x: int) -> int:
    if x <= 0:
        return 0
    return 1 if count_set_bits(x) == 1 else 0


def highest_bit_pos(x: int) -> int:
    pos = -1
    while x > 0:
        pos += 1
        x //= 2
    return pos


def lowest_bit_pos(x: int) -> int:
    if x == 0:
        return -1
    pos = 0
    while x % 2 == 0:
        pos += 1
        x //= 2
    return pos


def bit_at(x: int, i: int) -> int:
    return (x // pow2(i)) % 2


def extract_bits(x: int, start: int, count: int) -> int:
    return (x // pow2(start)) % pow2(count)


def set_bit(x: int, i: int) -> int:
    if bit_at(x, i) == 1:
        return x
    return x + pow2(i)


def clear_bit(x: int, i: int) -> int:
    if bit_at(x, i) == 0:
        return x
    return x - pow2(i)


def toggle_bit(x: int, i: int) -> int:
    if bit_at(x, i) == 1:
        return x - pow2(i)
    return x + pow2(i)


def popcount_range(lo: int, hi: int) -> int:
    total = 0
    for v in range(lo, hi + 1):
        total += count_set_bits(v)
    return total


def reverse_bits_n(x: int, n: int) -> int:
    result = 0
    for _ in range(n):
        result = result * 2 + x % 2
        x //= 2
    return result


def hamming_distance_bits(a: int, b: int) -> int:
    dist = 0
    while a > 0 or b > 0:
        if a % 2 != b % 2:
            dist += 1
        a //= 2
        b //= 2
    return dist


def next_power_of_two(x: int) -> int:
    p = 1
    while p < x:
        p = p * 2
    return p


def count_leading_zeros_64(x: int) -> int:
    if x == 0:
        return 64
    count = 64
    while x > 0:
        count -= 1
        x //= 2
    return count


def parity_bit(x: int) -> int:
    return count_set_bits(x) % 2


def is_bit_palindrome(x: int, n: int) -> int:
    return 1 if reverse_bits_n(x, n) == extract_bits(x, 0, n) else 0


def rotate_left_n(x: int, n: int, bits: int) -> int:
    width = pow2(bits)
    x %= width
    n %= bits
    hi = (x * pow2(n)) % width
    lo = x // pow2(bits - n)
    return hi + lo


def merge_bits(a: int, b: int, mask_count: int) -> int:
    m = pow2(mask_count)
    return a - a % m + b % m


def main() -> None:
    print("count_set_bits=" + str(count_set_bits(181)))
    print("is_power_of_two=" + str(is_power_of_two(64)))
    print("highest_bit_pos=" + str(highest_bit_pos(181)))
    print("lowest_bit_pos=" + str(lowest_bit_pos(180)))
    print("bit_at=" + str(bit_at(181, 2)))
    print("set_bit=" + str(set_bit(181, 1)))
    print("clear_bit=" + str(clear_bit(181, 0)))
    print("toggle_bit=" + str(toggle_bit(181, 3)))
    print("popcount_range=" + str(popcount_range(0, 7)))
    print("reverse_bits_n=" + str(reverse_bits_n(13, 4)))
    print("hamming_distance_bits=" + str(hamming_distance_bits(181, 90)))
    print("next_power_of_two=" + str(next_power_of_two(100)))
    print("count_leading_zeros_64=" + str(count_leading_zeros_64(1)))
    print("parity_bit=" + str(parity_bit(181)))
    print("is_bit_palindrome=" + str(is_bit_palindrome(9, 4)))
    print("rotate_left_n=" + str(rotate_left_n(1, 1, 4)))
    print("extract_bits=" + str(extract_bits(181, 2, 3)))
    print("merge_bits=" + str(merge_bits(240, 15, 4)))


if __name__ == "__main__":
    main()

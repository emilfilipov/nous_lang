// Cross-language bitwise suite (JavaScript). Bit manipulation over integers done
// with ARITHMETIC only: no bitwise operators (^ & | << >>) anywhere, to stay
// algorithm-identical to the Lullaby port, which models bit operations with arithmetic. Bits are
// read with % 2 and shifted with Math.trunc(x / 2) (or * a power of two).

function pow2(n) {
  let p = 1;
  for (let i = 1; i <= n; i++) {
    p = p * 2;
  }
  return p;
}

function count_set_bits(x) {
  let count = 0;
  while (x > 0) {
    count += x % 2;
    x = Math.trunc(x / 2);
  }
  return count;
}

function is_power_of_two(x) {
  if (x <= 0) {
    return 0;
  }
  return count_set_bits(x) === 1 ? 1 : 0;
}

function highest_bit_pos(x) {
  let pos = -1;
  while (x > 0) {
    pos += 1;
    x = Math.trunc(x / 2);
  }
  return pos;
}

function lowest_bit_pos(x) {
  if (x === 0) {
    return -1;
  }
  let pos = 0;
  while (x % 2 === 0) {
    pos += 1;
    x = Math.trunc(x / 2);
  }
  return pos;
}

function bit_at(x, i) {
  return Math.trunc(x / pow2(i)) % 2;
}

function extract_bits(x, start, count) {
  return Math.trunc(x / pow2(start)) % pow2(count);
}

function set_bit(x, i) {
  if (bit_at(x, i) === 1) {
    return x;
  }
  return x + pow2(i);
}

function clear_bit(x, i) {
  if (bit_at(x, i) === 0) {
    return x;
  }
  return x - pow2(i);
}

function toggle_bit(x, i) {
  if (bit_at(x, i) === 1) {
    return x - pow2(i);
  }
  return x + pow2(i);
}

function popcount_range(lo, hi) {
  let total = 0;
  for (let v = lo; v <= hi; v++) {
    total += count_set_bits(v);
  }
  return total;
}

function reverse_bits_n(x, n) {
  let result = 0;
  for (let i = 0; i < n; i++) {
    result = result * 2 + x % 2;
    x = Math.trunc(x / 2);
  }
  return result;
}

function hamming_distance_bits(a, b) {
  let dist = 0;
  while (a > 0 || b > 0) {
    if (a % 2 !== b % 2) {
      dist += 1;
    }
    a = Math.trunc(a / 2);
    b = Math.trunc(b / 2);
  }
  return dist;
}

function next_power_of_two(x) {
  let p = 1;
  while (p < x) {
    p = p * 2;
  }
  return p;
}

function count_leading_zeros_64(x) {
  if (x === 0) {
    return 64;
  }
  let count = 64;
  while (x > 0) {
    count -= 1;
    x = Math.trunc(x / 2);
  }
  return count;
}

function parity_bit(x) {
  return count_set_bits(x) % 2;
}

function is_bit_palindrome(x, n) {
  return reverse_bits_n(x, n) === extract_bits(x, 0, n) ? 1 : 0;
}

function rotate_left_n(x, n, bits) {
  const width = pow2(bits);
  x %= width;
  n %= bits;
  const hi = (x * pow2(n)) % width;
  const lo = Math.trunc(x / pow2(bits - n));
  return hi + lo;
}

function merge_bits(a, b, mask_count) {
  const m = pow2(mask_count);
  return a - a % m + b % m;
}

function main() {
  console.log("count_set_bits=" + count_set_bits(181));
  console.log("is_power_of_two=" + is_power_of_two(64));
  console.log("highest_bit_pos=" + highest_bit_pos(181));
  console.log("lowest_bit_pos=" + lowest_bit_pos(180));
  console.log("bit_at=" + bit_at(181, 2));
  console.log("set_bit=" + set_bit(181, 1));
  console.log("clear_bit=" + clear_bit(181, 0));
  console.log("toggle_bit=" + toggle_bit(181, 3));
  console.log("popcount_range=" + popcount_range(0, 7));
  console.log("reverse_bits_n=" + reverse_bits_n(13, 4));
  console.log("hamming_distance_bits=" + hamming_distance_bits(181, 90));
  console.log("next_power_of_two=" + next_power_of_two(100));
  console.log("count_leading_zeros_64=" + count_leading_zeros_64(1));
  console.log("parity_bit=" + parity_bit(181));
  console.log("is_bit_palindrome=" + is_bit_palindrome(9, 4));
  console.log("rotate_left_n=" + rotate_left_n(1, 1, 4));
  console.log("extract_bits=" + extract_bits(181, 2, 3));
  console.log("merge_bits=" + merge_bits(240, 15, 4));
}

main();

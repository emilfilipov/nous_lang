#include <cstdio>
using i64 = long long;
static i64 is_prime(i64 n){ if(n<2) return 0; for(i64 d=2; d*d<=n; ++d) if(n%d==0) return 0; return 1; }
static i64 count_primes_below(i64 n){ i64 c=0; for(i64 k=2;k<n;++k) c+=is_prime(k); return c; }
int main(){ std::printf("%lld\n", count_primes_below(300000)); }

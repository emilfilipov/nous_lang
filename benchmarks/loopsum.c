#include <stdio.h>
int main(void) {
    long long acc = 0;
    for (long long i = 0; i < 1000000000LL; i++) acc += i;
    printf("%lld\n", acc);
    return 0;
}

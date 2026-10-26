/* setjmp/longjmp lowered by clang -mllvm -wasm-enable-sjlj to wasm exception
   handling; runtime in ferrite_libc.c (__wasm_setjmp, __wasm_longjmp). */
#ifndef FERRITE_SETJMP_H
#define FERRITE_SETJMP_H
typedef long long jmp_buf[8];
int setjmp(jmp_buf) __attribute__((returns_twice));
void longjmp(jmp_buf, int) __attribute__((noreturn));
#define _setjmp setjmp
#define _longjmp longjmp
#endif

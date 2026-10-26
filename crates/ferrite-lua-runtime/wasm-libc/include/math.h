/* Provided by Rust's compiler-builtins (libm port) on wasm32-unknown-unknown. */
#ifndef FERRITE_MATH_H
#define FERRITE_MATH_H
#define HUGE_VAL (__builtin_huge_val())
#define HUGE_VALF (__builtin_huge_valf())
#define INFINITY (__builtin_inff())
#define NAN (__builtin_nanf(""))
#define FP_NAN 0
#define FP_INFINITE 1
#define FP_ZERO 2
#define FP_SUBNORMAL 3
#define FP_NORMAL 4
#define isnan(x) __builtin_isnan(x)
#define isinf(x) __builtin_isinf(x)
#define isfinite(x) __builtin_isfinite(x)
#define signbit(x) __builtin_signbit(x)
double floor(double);
double ceil(double);
double fabs(double);
double fmod(double, double);
double pow(double, double);
double sqrt(double);
double exp(double);
double log(double);
double log2(double);
double log10(double);
double sin(double);
double cos(double);
double tan(double);
double asin(double);
double acos(double);
double atan(double);
double atan2(double, double);
double sinh(double);
double cosh(double);
double tanh(double);
double frexp(double, int *);
double ldexp(double, int);
double modf(double, double *);
#endif

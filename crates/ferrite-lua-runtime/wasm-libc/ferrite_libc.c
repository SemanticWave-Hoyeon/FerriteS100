/*
 * Minimal C library for the vendored Lua 5.4 interpreter on
 * wasm32-unknown-unknown (ferrite-lua-runtime, browser builds only).
 *
 * Lua's core and the libraries ferrite opens (base, table, string, math,
 * utf8, coroutine, package) need string, ctype, formatting, number parsing,
 * an allocator and setjmp/longjmp. The io/os/debug libraries are linked but
 * never opened; their file and process functions fail cleanly here.
 *
 * Floating-point conversion (printf %e/%f/%g/%a and strtod) and the heap are
 * implemented in Rust (src/wasm_libc.rs): Rust's float formatting and parsing
 * are exact (correctly rounded, ties to even), like the native C libraries,
 * so Lua's number/string conversions give the same bytes as a native build.
 * Math functions (floor, fmod, pow, sin, ...) come from Rust's
 * compiler-builtins; memcpy/memmove/memset/memcmp likewise.
 */
#include <ctype.h>
#include <errno.h>
#include <limits.h>
#include <locale.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

int errno;

/* ---- Rust side (src/wasm_libc.rs) ---- */
/* Formats `value` for conversion `conv` (one of eEfFgGaA) with precision
   `precision` (-1: default) and the '#' flag; writes at most `cap` bytes of
   the result (sign included, no padding) and returns its full length. */
size_t ferrite_wasm_format_double(char *out, size_t cap, char conv, int precision, int alt,
                                  double value);
void ferrite_wasm_write(int stream, const char *data, size_t len);

/* ---- setjmp/longjmp runtime for clang's -wasm-enable-sjlj lowering ---- */
struct ferrite_jmp_buf {
  void *func_invocation_id;
  uint32_t label;
  struct {
    void *env;
    int val;
  } arg;
};

void __wasm_setjmp(void *env, uint32_t label, void *func_invocation_id) {
  struct ferrite_jmp_buf *jb = env;
  if (label == 0 || func_invocation_id == NULL) __builtin_trap();
  jb->func_invocation_id = func_invocation_id;
  jb->label = label;
}

uint32_t __wasm_setjmp_test(void *env, void *func_invocation_id) {
  struct ferrite_jmp_buf *jb = env;
  if (jb->label == 0 || func_invocation_id == NULL) __builtin_trap();
  return jb->func_invocation_id == func_invocation_id ? jb->label : 0;
}

void __wasm_longjmp(void *env, int val) {
  struct ferrite_jmp_buf *jb = env;
  if (val == 0) val = 1;
  jb->arg.env = env;
  jb->arg.val = val;
  __builtin_wasm_throw(1, &jb->arg); /* 1: the C longjmp tag */
}

/* The exception tag thrown by longjmp (wasi-libc defines it the same way). */
__asm__(".globl __c_longjmp\n"
        ".tagtype __c_longjmp i32\n"
        "__c_longjmp:\n");

/* ---- process ---- */
void abort(void) { __builtin_trap(); }
void exit(int status) {
  (void)status;
  __builtin_trap();
}
char *getenv(const char *name) {
  (void)name;
  return NULL;
}
int system(const char *command) { return command == NULL ? 0 : -1; }
/* table.sort's pivot randomization for large arrays (native builds mix in
   clock() and time()); a fixed value keeps runs reproducible. */
unsigned int arc4random(void) { return 0; }
int abs(int v) { return v < 0 ? -v : v; }
int mkstemp(char *t) {
  (void)t;
  errno = ENOSYS;
  return -1;
}
int close(int fd) {
  (void)fd;
  return -1;
}

/* ---- ctype (C locale, ASCII) ---- */
int isdigit(int c) { return c >= '0' && c <= '9'; }
int islower(int c) { return c >= 'a' && c <= 'z'; }
int isupper(int c) { return c >= 'A' && c <= 'Z'; }
int isalpha(int c) { return islower(c) || isupper(c); }
int isalnum(int c) { return isalpha(c) || isdigit(c); }
int isxdigit(int c) { return isdigit(c) || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F'); }
int isspace(int c) { return c == ' ' || (c >= '\t' && c <= '\r'); }
int iscntrl(int c) { return (c >= 0 && c < 32) || c == 127; }
int isprint(int c) { return c >= 32 && c < 127; }
int isgraph(int c) { return c > 32 && c < 127; }
int ispunct(int c) { return isgraph(c) && !isalnum(c); }
int tolower(int c) { return isupper(c) ? c + 32 : c; }
int toupper(int c) { return islower(c) ? c - 32 : c; }

/* ---- string ---- */
size_t strlen(const char *s) {
  const char *p = s;
  while (*p) p++;
  return (size_t)(p - s);
}
int strcmp(const char *a, const char *b) {
  while (*a && *a == *b) a++, b++;
  return (unsigned char)*a - (unsigned char)*b;
}
int strncmp(const char *a, const char *b, size_t n) {
  for (; n; n--, a++, b++) {
    if (*a != *b || !*a) return (unsigned char)*a - (unsigned char)*b;
  }
  return 0;
}
int strcoll(const char *a, const char *b) { return strcmp(a, b); }
char *strcpy(char *d, const char *s) {
  char *r = d;
  while ((*d++ = *s++)) {
  }
  return r;
}
char *strncpy(char *d, const char *s, size_t n) {
  size_t i = 0;
  for (; i < n && s[i]; i++) d[i] = s[i];
  for (; i < n; i++) d[i] = 0;
  return d;
}
char *strcat(char *d, const char *s) {
  strcpy(d + strlen(d), s);
  return d;
}
char *strchr(const char *s, int c) {
  for (;; s++) {
    if (*s == (char)c) return (char *)s;
    if (!*s) return NULL;
  }
}
char *strrchr(const char *s, int c) {
  const char *r = NULL;
  for (;; s++) {
    if (*s == (char)c) r = s;
    if (!*s) return (char *)r;
  }
}
void *memchr(const void *p, int c, size_t n) {
  const unsigned char *s = p;
  for (; n; n--, s++)
    if (*s == (unsigned char)c) return (void *)s;
  return NULL;
}
char *strstr(const char *h, const char *n) {
  size_t len = strlen(n);
  if (!len) return (char *)h;
  for (; *h; h++)
    if (*h == *n && !strncmp(h, n, len)) return (char *)h;
  return NULL;
}
size_t strspn(const char *s, const char *accept) {
  size_t i = 0;
  while (s[i] && strchr(accept, s[i])) i++;
  return i;
}
size_t strcspn(const char *s, const char *reject) {
  size_t i = 0;
  while (s[i] && !strchr(reject, s[i])) i++;
  return i;
}
char *strpbrk(const char *s, const char *accept) {
  s += strcspn(s, accept);
  return *s ? (char *)s : NULL;
}
char *strerror(int e) {
  switch (e) {
  case 0: return "No error information";
  case ENOENT: return "No such file or directory";
  case ENOMEM: return "Out of memory";
  case ENOSYS: return "Function not implemented";
  case EINVAL: return "Invalid argument";
  case ERANGE: return "Result not representable";
  default: return "Unknown error";
  }
}

/* ---- locale ---- */
static struct lconv c_lconv = {".", "", ""};
struct lconv *localeconv(void) { return &c_lconv; }
char *setlocale(int category, const char *locale) {
  (void)category;
  if (locale == NULL || !*locale || !strcmp(locale, "C") || !strcmp(locale, "POSIX"))
    return "C";
  return NULL;
}

/* ---- time (no clock in the interpreter: os is never opened) ---- */
time_t time(time_t *t) {
  if (t) *t = 0;
  return 0;
}
clock_t clock(void) { return (clock_t)-1; }
double difftime(time_t a, time_t b) { return (double)(a - b); }
time_t mktime(struct tm *tm) {
  (void)tm;
  return (time_t)-1;
}
struct tm *gmtime_r(const time_t *t, struct tm *r) {
  (void)t;
  (void)r;
  return NULL;
}
struct tm *localtime_r(const time_t *t, struct tm *r) {
  (void)t;
  (void)r;
  return NULL;
}
struct tm *gmtime(const time_t *t) {
  (void)t;
  return NULL;
}
struct tm *localtime(const time_t *t) {
  (void)t;
  return NULL;
}
size_t strftime(char *s, size_t n, const char *f, const struct tm *tm) {
  (void)s;
  (void)n;
  (void)f;
  (void)tm;
  return 0;
}

/* ---- numbers ---- */
/* Exact (bit-level); not among compiler-builtins' math exports. */
double frexp(double x, int *e) {
  union {
    double d;
    uint64_t u;
  } v = {x};
  int exponent = (int)(v.u >> 52 & 0x7ff);
  if (exponent == 0) {
    if (x == 0) {
      *e = 0;
      return x;
    }
    x = frexp(x * 0x1p64, e); /* subnormal */
    *e -= 64;
    return x;
  }
  if (exponent == 0x7ff) {
    *e = 0;
    return x;
  }
  *e = exponent - 1022;
  v.u = (v.u & 0x800fffffffffffffull) | 0x3fe0000000000000ull;
  return v.d;
}

static int digit_value(int c) {
  if (isdigit(c)) return c - '0';
  if (c >= 'a' && c <= 'z') return c - 'a' + 10;
  if (c >= 'A' && c <= 'Z') return c - 'A' + 10;
  return 99;
}
unsigned long strtoul(const char *s, char **end, int base) {
  const char *p = s;
  int neg = 0, any = 0;
  unsigned long v = 0;
  while (isspace((unsigned char)*p)) p++;
  if (*p == '+' || *p == '-') neg = *p++ == '-';
  if ((base == 0 || base == 16) && p[0] == '0' && (p[1] == 'x' || p[1] == 'X') &&
      isxdigit((unsigned char)p[2]))
    p += 2, base = 16;
  else if (base == 0)
    base = p[0] == '0' ? 8 : 10;
  for (; digit_value((unsigned char)*p) < base; p++, any = 1)
    v = v * (unsigned long)base + (unsigned long)digit_value((unsigned char)*p);
  if (end) *end = (char *)(any ? p : s);
  return neg ? -v : v;
}
long strtol(const char *s, char **end, int base) { return (long)strtoul(s, end, base); }
float strtof(const char *s, char **end) { return (float)strtod(s, end); }
long double strtold(const char *s, char **end) { return strtod(s, end); }

/* ---- formatted output ---- */
struct sink {
  char *buf;
  size_t cap;
  size_t len;
};

static void put(struct sink *o, const char *s, size_t n) {
  for (size_t i = 0; i < n; i++, o->len++)
    if (o->len + 1 < o->cap) o->buf[o->len] = s[i];
}
static void pad(struct sink *o, char c, int n) {
  for (; n > 0; n--) put(o, &c, 1);
}

/* Emit a converted field: prefix (sign/0x), zero padding and digits. */
static void field(struct sink *o, const char *prefix, size_t plen, const char *body, size_t blen,
                  int width, int left, int zero) {
  int total = (int)(plen + blen);
  if (!left && !zero) pad(o, ' ', width - total);
  put(o, prefix, plen);
  if (!left && zero) pad(o, '0', width - total);
  put(o, body, blen);
  if (left) pad(o, ' ', width - total);
}

int vsnprintf(char *buf, size_t cap, const char *fmt, va_list ap) {
  struct sink o = {buf, cap, 0};
  for (; *fmt; fmt++) {
    if (*fmt != '%') {
      put(&o, fmt, 1);
      continue;
    }
    fmt++;
    int left = 0, plus = 0, space = 0, alt = 0, zero = 0, width = 0, prec = -1;
    for (;; fmt++) {
      if (*fmt == '-') left = 1;
      else if (*fmt == '+') plus = 1;
      else if (*fmt == ' ') space = 1;
      else if (*fmt == '#') alt = 1;
      else if (*fmt == '0') zero = 1;
      else break;
    }
    if (*fmt == '*') {
      width = va_arg(ap, int);
      if (width < 0) left = 1, width = -width;
      fmt++;
    } else
      while (isdigit((unsigned char)*fmt)) width = width * 10 + (*fmt++ - '0');
    if (*fmt == '.') {
      fmt++;
      prec = 0;
      if (*fmt == '*') {
        prec = va_arg(ap, int);
        fmt++;
      } else
        while (isdigit((unsigned char)*fmt)) prec = prec * 10 + (*fmt++ - '0');
    }
    int size = 0; /* 0 int, 1 long, 2 long long/intmax/size, -1 short, -2 char */
    for (;; fmt++) {
      if (*fmt == 'l') size = size == 1 ? 2 : 1;
      else if (*fmt == 'h') size = size == -1 ? -2 : -1;
      else if (*fmt == 'j' || *fmt == 'z' || *fmt == 't') size = (*fmt == 'j') ? 2 : 1;
      else if (*fmt == 'L') size = 3;
      else break;
    }
    if (left) zero = 0;
    char conv = *fmt;
    char tmp[512];
    switch (conv) {
    case '%':
      put(&o, "%", 1);
      break;
    case 'c': {
      char c = (char)va_arg(ap, int);
      field(&o, "", 0, &c, 1, width, left, 0);
      break;
    }
    case 's': {
      const char *s = va_arg(ap, const char *);
      if (!s) s = "(null)";
      size_t n = 0;
      while ((prec < 0 || n < (size_t)prec) && s[n]) n++;
      field(&o, "", 0, s, n, width, left, 0);
      break;
    }
    case 'd':
    case 'i':
    case 'u':
    case 'o':
    case 'x':
    case 'X':
    case 'p': {
      unsigned long long u;
      int neg = 0;
      int base = conv == 'o' ? 8 : (conv == 'x' || conv == 'X' || conv == 'p') ? 16 : 10;
      if (conv == 'p') {
        u = (unsigned long long)(uintptr_t)va_arg(ap, void *);
        alt = 1;
      } else if (conv == 'd' || conv == 'i') {
        long long v = size >= 2 ? va_arg(ap, long long) : size == 1 ? va_arg(ap, long) : va_arg(ap, int);
        if (size == -1) v = (short)v;
        if (size == -2) v = (signed char)v;
        neg = v < 0;
        u = neg ? 0ull - (unsigned long long)v : (unsigned long long)v;
      } else {
        u = size >= 2 ? va_arg(ap, unsigned long long)
            : size == 1 ? va_arg(ap, unsigned long)
                        : va_arg(ap, unsigned int);
        if (size == -1) u = (unsigned short)u;
        if (size == -2) u = (unsigned char)u;
      }
      const char *digits = conv == 'X' ? "0123456789ABCDEF" : "0123456789abcdef";
      char *end = tmp + sizeof tmp, *p = end;
      for (unsigned long long v = u; v; v /= (unsigned)base) *--p = digits[v % (unsigned)base];
      int ndig = (int)(end - p);
      if (prec >= 0) zero = 0;
      int minimum = prec < 0 ? 1 : prec;
      if (conv == 'o' && alt && (ndig >= minimum) && !(ndig && *p == '0')) minimum = ndig + 1;
      while (ndig < minimum) *--p = '0', ndig++;
      char prefix[3];
      size_t plen = 0;
      if (conv == 'd' || conv == 'i') {
        if (neg) prefix[plen++] = '-';
        else if (plus) prefix[plen++] = '+';
        else if (space) prefix[plen++] = ' ';
      } else if ((conv == 'x' || conv == 'X') && alt && u) {
        prefix[plen++] = '0';
        prefix[plen++] = conv;
      } else if (conv == 'p') {
        prefix[plen++] = '0';
        prefix[plen++] = 'x';
      }
      field(&o, prefix, plen, p, (size_t)ndig, width, left, zero);
      break;
    }
    case 'e':
    case 'E':
    case 'f':
    case 'F':
    case 'g':
    case 'G':
    case 'a':
    case 'A': {
      double v = size == 3 ? (double)va_arg(ap, long double) : va_arg(ap, double);
      size_t n = ferrite_wasm_format_double(tmp, sizeof tmp, conv, prec, alt, v);
      if (n > sizeof tmp) n = sizeof tmp;
      const char *body = tmp;
      char prefix[1];
      size_t plen = 0;
      if (body[0] == '-') {
        prefix[plen++] = '-';
        body++, n--;
      } else if (plus)
        prefix[plen++] = '+';
      else if (space)
        prefix[plen++] = ' ';
      int finite = isdigit((unsigned char)body[0]);
      if (finite && zero && (conv == 'a' || conv == 'A')) {
        /* zero padding goes after the 0x prefix */
        put(&o, prefix, plen);
        put(&o, body, 2);
        int total = (int)(plen + n);
        pad(&o, '0', width - total);
        put(&o, body + 2, n - 2);
      } else
        field(&o, prefix, plen, body, n, width, left, zero && finite);
      break;
    }
    case 'n':
      *va_arg(ap, int *) = (int)o.len;
      break;
    default:
      put(&o, "%", 1);
      if (conv) put(&o, &conv, 1);
      else fmt--;
      break;
    }
  }
  if (cap) buf[o.len < cap ? o.len : cap - 1] = 0;
  return (int)o.len;
}

int snprintf(char *buf, size_t cap, const char *fmt, ...) {
  va_list ap;
  va_start(ap, fmt);
  int n = vsnprintf(buf, cap, fmt, ap);
  va_end(ap);
  return n;
}
int sprintf(char *buf, const char *fmt, ...) {
  va_list ap;
  va_start(ap, fmt);
  int n = vsnprintf(buf, (size_t)INT_MAX, fmt, ap);
  va_end(ap);
  return n;
}

/* ---- streams: stdout/stderr go to the host log, there are no files ---- */
struct FerriteFile {
  int fd;
};
static struct FerriteFile files[3] = {{0}, {1}, {2}};
FILE *const stdin = &files[0];
FILE *const stdout = &files[1];
FILE *const stderr = &files[2];

static FILE *no_file(void) {
  errno = ENOENT;
  return NULL;
}
FILE *fopen(const char *p, const char *m) {
  (void)p;
  (void)m;
  return no_file();
}
FILE *freopen(const char *p, const char *m, FILE *f) {
  (void)p;
  (void)m;
  (void)f;
  return no_file();
}
FILE *tmpfile(void) { return no_file(); }
FILE *popen(const char *c, const char *m) {
  (void)c;
  (void)m;
  return no_file();
}
int pclose(FILE *f) {
  (void)f;
  return -1;
}
int fclose(FILE *f) {
  (void)f;
  return 0;
}
int fflush(FILE *f) {
  (void)f;
  return 0;
}
size_t fwrite(const void *p, size_t size, size_t n, FILE *f) {
  if (f && f->fd > 0) ferrite_wasm_write(f->fd, p, size * n);
  return n;
}
int fputs(const char *s, FILE *f) {
  fwrite(s, 1, strlen(s), f);
  return 0;
}
int fputc(int c, FILE *f) {
  char ch = (char)c;
  fwrite(&ch, 1, 1, f);
  return (unsigned char)c;
}
int putc(int c, FILE *f) { return fputc(c, f); }
size_t fread(void *p, size_t size, size_t n, FILE *f) {
  (void)p;
  (void)size;
  (void)n;
  (void)f;
  return 0;
}
int getc(FILE *f) {
  (void)f;
  return EOF;
}
int fgetc(FILE *f) { return getc(f); }
int ungetc(int c, FILE *f) {
  (void)c;
  (void)f;
  return EOF;
}
char *fgets(char *s, int n, FILE *f) {
  (void)s;
  (void)n;
  (void)f;
  return NULL;
}
int ferror(FILE *f) {
  (void)f;
  return 0;
}
int feof(FILE *f) {
  (void)f;
  return 1;
}
void clearerr(FILE *f) { (void)f; }
int fseek(FILE *f, long o, int w) {
  (void)f;
  (void)o;
  (void)w;
  return -1;
}
long ftell(FILE *f) {
  (void)f;
  return -1;
}
int fseeko(FILE *f, off_t o, int w) {
  (void)f;
  (void)o;
  (void)w;
  return -1;
}
off_t ftello(FILE *f) {
  (void)f;
  return -1;
}
int setvbuf(FILE *f, char *b, int m, size_t s) {
  (void)f;
  (void)b;
  (void)m;
  (void)s;
  return 0;
}
int remove(const char *p) {
  (void)p;
  errno = ENOSYS;
  return -1;
}
int rename(const char *a, const char *b) {
  (void)a;
  (void)b;
  errno = ENOSYS;
  return -1;
}
char *tmpnam(char *s) {
  (void)s;
  return NULL;
}
int vfprintf(FILE *f, const char *fmt, va_list ap) {
  char buf[1024];
  int n = vsnprintf(buf, sizeof buf, fmt, ap);
  fwrite(buf, 1, (size_t)n < sizeof buf ? (size_t)n : sizeof buf - 1, f);
  return n;
}
int fprintf(FILE *f, const char *fmt, ...) {
  va_list ap;
  va_start(ap, fmt);
  int n = vfprintf(f, fmt, ap);
  va_end(ap);
  return n;
}
int printf(const char *fmt, ...) {
  va_list ap;
  va_start(ap, fmt);
  int n = vfprintf(stdout, fmt, ap);
  va_end(ap);
  return n;
}

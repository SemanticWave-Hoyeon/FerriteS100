#ifndef FERRITE_STDLIB_H
#define FERRITE_STDLIB_H
#include <stddef.h>
#define EXIT_SUCCESS 0
#define EXIT_FAILURE 1
void *malloc(size_t);
void *calloc(size_t, size_t);
void *realloc(void *, size_t);
void free(void *);
void abort(void) __attribute__((noreturn));
void exit(int) __attribute__((noreturn));
char *getenv(const char *);
int system(const char *);
double strtod(const char *, char **);
float strtof(const char *, char **);
long double strtold(const char *, char **);
long strtol(const char *, char **, int);
unsigned long strtoul(const char *, char **, int);
int abs(int);
int mkstemp(char *);
unsigned int arc4random(void);
#endif

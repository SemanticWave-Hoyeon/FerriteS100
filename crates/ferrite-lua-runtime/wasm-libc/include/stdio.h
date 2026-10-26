/* Minimal stdio for Lua on wasm32-unknown-unknown (ferrite-lua-runtime).
   No files: fopen fails, stdout/stderr discard. Formatting is real. */
#ifndef FERRITE_STDIO_H
#define FERRITE_STDIO_H
#include <stdarg.h>
#include <stddef.h>
#include <sys/types.h>
typedef struct FerriteFile FILE;
extern FILE *const stdin;
extern FILE *const stdout;
extern FILE *const stderr;
#define EOF (-1)
#define BUFSIZ 1024
#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2
#define _IOFBF 0
#define _IOLBF 1
#define _IONBF 2
#define L_tmpnam 32
#define FILENAME_MAX 4096
FILE *fopen(const char *, const char *);
FILE *freopen(const char *, const char *, FILE *);
FILE *tmpfile(void);
FILE *popen(const char *, const char *);
int pclose(FILE *);
int fclose(FILE *);
int fflush(FILE *);
size_t fread(void *, size_t, size_t, FILE *);
size_t fwrite(const void *, size_t, size_t, FILE *);
int fputs(const char *, FILE *);
int fputc(int, FILE *);
int putc(int, FILE *);
int getc(FILE *);
int fgetc(FILE *);
int ungetc(int, FILE *);
char *fgets(char *, int, FILE *);
int ferror(FILE *);
int feof(FILE *);
void clearerr(FILE *);
int fseek(FILE *, long, int);
long ftell(FILE *);
int fseeko(FILE *, off_t, int);
off_t ftello(FILE *);
int setvbuf(FILE *, char *, int, size_t);
int remove(const char *);
int rename(const char *, const char *);
char *tmpnam(char *);
int printf(const char *, ...);
int fprintf(FILE *, const char *, ...);
int vfprintf(FILE *, const char *, va_list);
int sprintf(char *, const char *, ...);
int snprintf(char *, size_t, const char *, ...);
int vsnprintf(char *, size_t, const char *, va_list);
#endif

#ifndef FERRITE_LOCALE_H
#define FERRITE_LOCALE_H
struct lconv { char *decimal_point; char *thousands_sep; char *grouping; };
#define LC_ALL 0
#define LC_COLLATE 1
#define LC_CTYPE 2
#define LC_MONETARY 3
#define LC_NUMERIC 4
#define LC_TIME 5
struct lconv *localeconv(void);
char *setlocale(int, const char *);
#endif

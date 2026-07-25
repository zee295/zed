#include <stddef.h>
#include <stdarg.h>

#ifndef NULL
#define NULL ((void *)0)
#endif

struct __sFILE;
typedef struct __sFILE FILE;

FILE *stdout = NULL;
FILE *stderr = NULL;

static int write_num(char *buf, size_t bufsize, size_t *pos, unsigned long long v, int base, int width, int zero, int upper) {
    char tmp[32];
    int n = 0;
    do {
        int d = v % base;
        tmp[n++] = (d < 10 ? '0' + d : (upper ? 'A' : 'a') + d - 10);
        v /= base;
    } while (v);
    while (width > n) {
        if (*pos < bufsize) buf[*pos] = zero ? '0' : ' ';
        (*pos)++;
        width--;
    }
    while (n--) {
        if (*pos < bufsize) buf[*pos] = tmp[n];
        (*pos)++;
    }
    return 0;
}

int vsnprintf(char *str, size_t size, const char *format, va_list ap) {
    size_t pos = 0;
    for (const char *p = format; *p; p++) {
        if (*p != '%') {
            if (pos < size) str[pos] = *p;
            pos++;
            continue;
        }
        p++;
        int zero = 0;
        int width = 0;
        if (*p == '0') { zero = 1; p++; }
        while (*p >= '0' && *p <= '9') { width = width * 10 + (*p - '0'); p++; }
        if (*p == 's') {
            const char *s = va_arg(ap, const char *);
            if (!s) s = "(null)";
            while (*s) {
                if (pos < size) str[pos] = *s;
                pos++;
                s++;
            }
        } else if (*p == 'd' || *p == 'i') {
            int v = va_arg(ap, int);
            if (v < 0) {
                if (pos < size) str[pos] = '-';
                pos++;
                v = -v;
            }
            write_num(str, size, &pos, (unsigned)v, 10, width, zero, 0);
        } else if (*p == 'u') {
            unsigned v = va_arg(ap, unsigned);
            write_num(str, size, &pos, v, 10, width, zero, 0);
        } else if (*p == 'x' || *p == 'X') {
            unsigned v = va_arg(ap, unsigned);
            write_num(str, size, &pos, v, 16, width, zero, *p == 'X');
        } else if (*p == 'p') {
            void *v = va_arg(ap, void *);
            if (pos < size) str[pos] = '0';
            pos++;
            if (pos < size) str[pos] = 'x';
            pos++;
            write_num(str, size, &pos, (size_t)v, 16, 0, 0, 0);
        } else if (*p == 'c') {
            int c = va_arg(ap, int);
            if (pos < size) str[pos] = (char)c;
            pos++;
        } else if (*p == '%') {
            if (pos < size) str[pos] = '%';
            pos++;
        } else if (*p == 'z' && *(p+1) == 'u') {
            p++;
            size_t v = va_arg(ap, size_t);
            write_num(str, size, &pos, v, 10, width, zero, 0);
        } else {
            if (pos < size) str[pos] = *p;
            pos++;
        }
    }
    if (size > 0) {
        if (pos < size) str[pos] = '\0';
        else str[size - 1] = '\0';
    }
    return (int)pos;
}

int snprintf(char *str, size_t size, const char *format, ...) {
    va_list ap;
    va_start(ap, format);
    int r = vsnprintf(str, size, format, ap);
    va_end(ap);
    return r;
}

int fprintf(FILE *stream, const char *format, ...) {
    (void)stream;
    (void)format;
    return 0;
}

int vfprintf(FILE *stream, const char *format, va_list ap) {
    (void)stream;
    (void)format;
    (void)ap;
    return 0;
}

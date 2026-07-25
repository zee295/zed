#include <stddef.h>
#include <string.h>

#ifndef NULL
#define NULL ((void *)0)
#endif

#define HEAP_SIZE (32 * 1024 * 1024)
static unsigned char heap[HEAP_SIZE];
static size_t heap_offset = 0;

typedef struct block {
    size_t size;
    struct block *next;
} block_t;

static block_t *free_list = NULL;

static void *allocate(size_t size) {
    if (size == 0) size = 1;
    size_t align = sizeof(void *);
    size = (size + align - 1) & ~(align - 1);

    block_t **prev = &free_list;
    for (block_t *b = free_list; b; prev = &b->next, b = b->next) {
        if (b->size >= size) {
            *prev = b->next;
            return (void *)(b + 1);
        }
    }

    size_t total = size + sizeof(block_t);
    if (heap_offset + total > HEAP_SIZE) {
        return NULL;
    }
    block_t *b = (block_t *)(heap + heap_offset);
    heap_offset += total;
    b->size = size;
    b->next = NULL;
    return (void *)(b + 1);
}

void *malloc(size_t size) { return allocate(size); }

void free(void *ptr) {
    if (!ptr) return;
    block_t *b = (block_t *)ptr - 1;
    b->next = free_list;
    free_list = b;
}

void *calloc(size_t nmemb, size_t size) {
    size_t total = nmemb * size;
    void *p = allocate(total);
    if (p) memset(p, 0, total);
    return p;
}

void *realloc(void *ptr, size_t size) {
    if (!ptr) return allocate(size);
    block_t *b = (block_t *)ptr - 1;
    if (b->size >= size) return ptr;
    void *p = allocate(size);
    if (p) {
        memcpy(p, ptr, b->size);
        free(ptr);
    }
    return p;
}

void abort(void) {
    __builtin_trap();
    __builtin_unreachable();
}

void exit(int status) {
    (void)status;
    __builtin_trap();
    __builtin_unreachable();
}

static void swap(unsigned char *a, unsigned char *b, size_t size) {
    while (size--) {
        unsigned char t = *a;
        *a++ = *b;
        *b++ = t;
    }
}

void qsort(void *base, size_t nmemb, size_t size,
           int (*cmp)(const void *, const void *)) {
    unsigned char *arr = base;
    for (size_t i = 1; i < nmemb; i++) {
        for (size_t j = i; j > 0 && cmp(arr + (j - 1) * size, arr + j * size) > 0; j--) {
            swap(arr + (j - 1) * size, arr + j * size, size);
        }
    }
}

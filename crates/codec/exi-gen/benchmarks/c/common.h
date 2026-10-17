/* Shared by the C harnesses: the corpus, the clock and the report. */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

typedef struct { char name[96]; char group[8]; char schema[8]; uint8_t bytes[1024]; size_t len; } Vector;

static double now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e9 + t.tv_nsec;
}

/* Reads the vectors of `group` from corpus.txt (name, group, hex per line). */
static size_t load(const char* path, const char* group, Vector* out, size_t max) {
    FILE* f = fopen(path, "r");
    if (!f) { perror(path); exit(2); }
    static char line[4096];
    size_t n = 0;
    while (fgets(line, sizeof line, f) && n < max) {
        char* name = strtok(line, "\t");
        char* g = strtok(NULL, "\t");
        char* hex = strtok(NULL, "\t\n");
        char* schema = strtok(NULL, "\t\n");
        if (!name || !g || !hex || !schema || strcmp(g, group) != 0) continue;
        Vector* v = &out[n++];
        snprintf(v->name, sizeof v->name, "%s", name);
        snprintf(v->group, sizeof v->group, "%s", g);
        snprintf(v->schema, sizeof v->schema, "%s", schema);
        v->len = strlen(hex) / 2;
        for (size_t i = 0; i < v->len; i++) { unsigned b; sscanf(hex + 2 * i, "%2x", &b); v->bytes[i] = (uint8_t)b; }
    }
    fclose(f);
    return n;
}

/* Calls `f` until about 3 ms have passed (at least 3 calls); nanoseconds per call. */
#define TIME_IT(result, body) do { \
    double t0 = now_ns(), t1; long calls = 0; \
    do { for (int k = 0; k < 16; k++) { body; } calls += 16; t1 = now_ns(); } while (t1 - t0 < 3e6); \
    result = (t1 - t0) / calls; } while (0)

typedef struct { int ok, total; double decode_ns, encode_ns; } Tally;

static void report(const char* lib, const char* group, const Tally* t, size_t doc_bytes) {
    printf("RESULT\t%s\t%s\t%d\t%d\t%.1f\t%.1f\t%zu\n", lib, group, t->ok, t->total, t->ok ? t->decode_ns / t->ok : 0.0, t->ok ? t->encode_ns / t->ok : 0.0, doc_bytes);
}

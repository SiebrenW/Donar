/* OpenV2G (Siemens, 2025 master): DIN SPEC 70121 and ISO 15118-2:2013 ("iso1"). Decodes every
 * document of the corpus into the library's struct, encodes it again and compares the bytes. */
#include "common.h"
#include "codec/EXITypes.h"
#include "din/dinEXIDatatypes.h"
#include "din/dinEXIDatatypesDecoder.h"
#include "din/dinEXIDatatypesEncoder.h"
#include "iso1/iso1EXIDatatypes.h"
#include "iso1/iso1EXIDatatypesDecoder.h"
#include "iso1/iso1EXIDatatypesEncoder.h"

static Vector corpus[200];

#define RUN(NAME, DOC, DECODE, ENCODE) \
static void NAME(const char* group, size_t count) { \
    static DOC doc; \
    Tally t = {0}; \
    for (size_t i = 0; i < count; i++) { \
        Vector* v = &corpus[i]; \
        t.total++; \
        uint8_t out[1024]; size_t pin = 0, pout = 0; \
        bitstream_t in = { .size = v->len, .data = v->bytes, .pos = &pin }; \
        bitstream_t os = { .size = sizeof out, .data = out, .pos = &pout }; \
        int e = DECODE(&in, &doc); \
        if (e) { printf("MISMATCH\topenv2g\t%s\t%s\tdecode error %d\n", group, v->name, e); continue; } \
        e = ENCODE(&os, &doc); \
        if (e) { printf("MISMATCH\topenv2g\t%s\t%s\tencode error %d\n", group, v->name, e); continue; } \
        if (pout != v->len || memcmp(out, v->bytes, v->len)) { printf("MISMATCH\topenv2g\t%s\t%s\tre-encoded differently (%zu bytes, was %zu)\n", group, v->name, pout, v->len); continue; } \
        t.ok++; \
        double d, n; \
        TIME_IT(d, { pin = 0; DECODE(&in, &doc); }); \
        TIME_IT(n, { pout = 0; ENCODE(&os, &doc); }); \
        t.decode_ns += d; t.encode_ns += n; \
    } \
    report("openv2g", group, &t, sizeof(DOC)); \
}

RUN(run_din, struct dinEXIDocument, decode_dinExiDocument, encode_dinExiDocument)
RUN(run_iso2, struct iso1EXIDocument, decode_iso1ExiDocument, encode_iso1ExiDocument)

int main(int argc, char** argv) {
    const char* path = argc > 1 ? argv[1] : "corpus.txt";
    (void)path;
#ifdef P_DIN
    size_t n = load(path, "din", corpus, 200);
    run_din("din", n);
#endif
#ifdef P_ISO2
    size_t m = load(path, "iso2", corpus, 200);
    run_iso2("iso2", m);
#endif
    return 0;
}

/* libcbv2g (EVerest/Pionix, master): DIN SPEC 70121, ISO 15118-2:2013 and ISO 15118-20 (the
 * common messages, DC and AC schemas). Decodes every document of the corpus, encodes it again
 * and compares the bytes. */
#include "common.h"
#include <cbv2g/common/exi_bitstream.h>
#include <cbv2g/din/din_msgDefDecoder.h>
#include <cbv2g/din/din_msgDefEncoder.h>
#include <cbv2g/iso_2/iso2_msgDefDecoder.h>
#include <cbv2g/iso_2/iso2_msgDefEncoder.h>
#include <cbv2g/iso_20/iso20_CommonMessages_Decoder.h>
#include <cbv2g/iso_20/iso20_CommonMessages_Encoder.h>
#include <cbv2g/iso_20/iso20_DC_Decoder.h>
#include <cbv2g/iso_20/iso20_DC_Encoder.h>
#include <cbv2g/iso_20/iso20_AC_Decoder.h>
#include <cbv2g/iso_20/iso20_AC_Encoder.h>

static Vector corpus[200];

/* One schema: its document type, its decoder and its encoder. */
#define SCHEMA(NAME, DOC, DECODE, ENCODE) \
static int NAME##_decode(Vector* v, DOC* doc) { exi_bitstream_t s; exi_bitstream_init(&s, v->bytes, v->len, 0, NULL); return DECODE(&s, doc); } \
static int NAME##_encode(uint8_t* out, size_t cap, DOC* doc, size_t* len) { exi_bitstream_t s; exi_bitstream_init(&s, out, cap, 0, NULL); int e = ENCODE(&s, doc); *len = exi_bitstream_get_length(&s); return e; }

SCHEMA(din, struct din_exiDocument, decode_din_exiDocument, encode_din_exiDocument)
SCHEMA(iso2, struct iso2_exiDocument, decode_iso2_exiDocument, encode_iso2_exiDocument)
SCHEMA(common, struct iso20_exiDocument, decode_iso20_exiDocument, encode_iso20_exiDocument)
SCHEMA(dc, struct iso20_dc_exiDocument, decode_iso20_dc_exiDocument, encode_iso20_dc_exiDocument)
SCHEMA(ac, struct iso20_ac_exiDocument, decode_iso20_ac_exiDocument, encode_iso20_ac_exiDocument)

/* Why a document did not round-trip: the decode error, or what the encoder did. */
#define WHY(NAME, DOC, v, buf) do { \
    static DOC d; size_t len = 0; int de = NAME##_decode(v, &d); \
    if (de) snprintf(buf, sizeof buf, "decode error %d", de); \
    else { uint8_t o[1024]; int ee = NAME##_encode(o, 1024, &d, &len); \
        if (ee) snprintf(buf, sizeof buf, "encode error %d", ee); \
        else snprintf(buf, sizeof buf, "re-encoded differently (%zu bytes, was %zu)", len, v->len); } } while (0)

#define CHECK(NAME, DOC, group, v, t, out, DONE) do { \
    static DOC doc; size_t len; \
    if (NAME##_decode(v, &doc) == 0 && NAME##_encode(out, 1024, &doc, &len) == 0 && len == v->len && !memcmp(out, v->bytes, len)) { \
        double d, n; \
        TIME_IT(d, { NAME##_decode(v, &doc); }); \
        TIME_IT(n, { NAME##_encode(out, 1024, &doc, &len); }); \
        t.ok++; t.decode_ns += d; t.encode_ns += n; DONE = 1; \
    } } while (0)

static void run_plain(const char* group, size_t count, int which) {
    Tally t = {0};
    for (size_t i = 0; i < count; i++) {
        Vector* v = &corpus[i];
        uint8_t out[1024];
        int done = 0;
        t.total++;
        if (which == 0) CHECK(din, struct din_exiDocument, group, v, t, out, done);
        else CHECK(iso2, struct iso2_exiDocument, group, v, t, out, done);
        if (!done) {
            char why[96];
            if (which == 0) WHY(din, struct din_exiDocument, v, why); else WHY(iso2, struct iso2_exiDocument, v, why);
            printf("MISMATCH\tcbv2g\t%s\t%s\t%s\n", group, v->name, why);
        }
    }
    report("cbv2g", group, &t, which == 0 ? sizeof(struct din_exiDocument) : sizeof(struct iso2_exiDocument));
}

static void run_iso20(const char* group, size_t count) {
    Tally t = {0};
    for (size_t i = 0; i < count; i++) {
        Vector* v = &corpus[i];
        uint8_t out[1024];
        int done = 0;
        t.total++;
        /* The schema of a document is told by the payload type on the wire; the corpus carries it. */
        if (!strcmp(v->schema, "common")) CHECK(common, struct iso20_exiDocument, group, v, t, out, done);
        if (!strcmp(v->schema, "dc")) CHECK(dc, struct iso20_dc_exiDocument, group, v, t, out, done);
        if (!strcmp(v->schema, "ac")) CHECK(ac, struct iso20_ac_exiDocument, group, v, t, out, done);
        if (!done) {
            char why[96];
            if (!strcmp(v->schema, "common")) WHY(common, struct iso20_exiDocument, v, why);
            else if (!strcmp(v->schema, "dc")) WHY(dc, struct iso20_dc_exiDocument, v, why);
            else WHY(ac, struct iso20_ac_exiDocument, v, why);
            printf("MISMATCH\tcbv2g\t%s\t%s\t%s\n", group, v->name, why);
        }
    }
    size_t big = sizeof(struct iso20_exiDocument);
    if (sizeof(struct iso20_dc_exiDocument) > big) big = sizeof(struct iso20_dc_exiDocument);
    if (sizeof(struct iso20_ac_exiDocument) > big) big = sizeof(struct iso20_ac_exiDocument);
    report("cbv2g", group, &t, big);
}

int main(int argc, char** argv) {
    const char* path = argc > 1 ? argv[1] : "corpus.txt";
    (void)path;
#ifdef P_DIN
    size_t n = load(path, "din", corpus, 200);
    run_plain("din", n, 0);
#endif
#ifdef P_ISO2
    size_t m = load(path, "iso2", corpus, 200);
    run_plain("iso2", m, 1);
#endif
#ifdef P_ISO20
    size_t k = load(path, "iso20", corpus, 200);
    run_iso20("iso20", k);
#endif
    return 0;
}

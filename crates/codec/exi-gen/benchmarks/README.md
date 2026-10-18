# Benchmarks: this EXI codec against the others

Compares the codecs that `exi-gen` generates with the three implementations people use for ISO 15118: **EXIficient** (Java, the reference the vectors come from), **OpenV2G** (Siemens, C) and **libcbv2g** (EVerest, C). The numbers are in [RESULTS.md](RESULTS.md); `run.sh` makes them again.

```sh
cargo xtask setup && cargo xtask vectors                        # schemas, EXIficient jars, reference documents
git clone https://github.com/Martin-P/OpenV2G data/bench/OpenV2G
git clone https://github.com/EVerest/libcbv2g data/bench/libcbv2g
crates/codec/exi-gen/benchmarks/run.sh                           # needs a JDK and a C compiler; RUNS=5 by default
```

## What is measured

**The corpus** is `corpus.txt`: the 173 reference documents of `cargo xtask vectors`, EXIficient's output for the XML in `crates/xtask/vectors` (all the message types of DIN SPEC 70121, ISO 15118-2:2013 and ISO 15118-20, with optional elements present and absent, empty strings, the longest lists). Schema-informed, bit-packed, default fidelity, `valuePartitionCapacity` 0, no EXI header options. `corpus.py` makes it; the fourth column is the schema of an ISO 15118-20 document, which on the wire the V2GTP payload type tells (all libraries are told, none has to guess).

**Round trip** (the first table): each library decodes every document into its own representation and encodes that to bytes; the bytes have to be exactly the document. This is the interoperability check: the other implementations' encoders write what EXIficient writes, or they do not. A failure is reported with the library's own error code; the documents that fail are edge cases (`*_empty*`: zero-length strings; `*_max`, `*_serials`: the longest lists; `certificate_*`: long binary values). Why each fails was not investigated in the libraries' code: limits of their fixed-size structs and event paths they do not implement are the likely causes.

**Speed**: nanoseconds per document, decode and encode separately, over the documents that round-tripped, the fastest of `RUNS` runs (Java: two runs of a warmed-up JVM). Each call is repeated for at least 3 ms (20 ms for Java after a warm-up). Decoding builds the library's own representation: a C struct, a Rust value whose strings and lists live in a caller-provided arena (reset between calls, not cleared), a list of recorded SAX events for EXIficient (its API is SAX/DOM; the events are what its decoder produces and its encoder takes). Encoding writes the representation decoded once. Not included: parsing or printing XML, building a message by hand.

**Memory**: what the decoded message needs. The C libraries use a struct sized for the largest message of the schema (`sizeof` of the document type); this crate uses the arena bytes the document needs (the most over the corpus). Heap and stack of the Java library are not measured.

**Code size**: a harness per protocol is built for size (`-Os`, or Rust `opt-level = "s"` with LTO and `panic = "abort"`; unused code removed by the linker) and the size of the same harness with no protocol is subtracted: code and constant data (`__text`, `__const`, `__cstring`, literals) that a round trip of that protocol pulls in. The harnesses differ in their own glue (Rust's `Debug` for error values is in every Rust figure; a few kilobytes), which the subtraction does not remove.

## Reading the results

- **Message size is small**: the median document is 18 to 31 bytes, so fixed costs dominate and the speed figures are per message, not per megabyte. They say nothing about large documents.
- **One machine, one compiler**: Apple M5 Pro (arm64), Apple clang, rustc 1.94, OpenJDK 27 (see the setup table in RESULTS.md). Other CPUs and compilers may change the ordering between the C libraries and this crate (they are within a factor of two of each other here); the order of magnitude to Java is unlikely to.
- **The C libraries are set up as shipped.** OpenV2G's DIN codec is off in its headers; `c/build.sh` switches it on in a copy (`DEPLOY_DIN_CODEC`, and the XML signature codec its ISO 15118-2 types use). OpenV2G has no ISO 15118-20; libcbv2g's three ISO 15118-20 schemas are the common messages, DC and AC (no ACDP, WPT).
- **A failure is a statement about this corpus**, not about every stream a stack sees: the libraries are used productively with the documents their users send. The failing documents are listed so that you can check whether your stack sends such documents.
- **Some corpus documents are schema-valid but outside what the standards use.** DIN SPEC 70121 does not use `ServiceList` (V2G-DC-549), `ServiceName` and `ServiceScope` (V2G-DC-628, 629, 622) and allows only `ExternalPayment` (V2G-DC-633), so a library that cuts the `ServiceList` to one entry or the `PaymentOptions` to what DIN allows loses nothing in DIN use. That covers the two DIN failures of libcbv2g (`din_service_discovery_res_*`). What stays a difference of the coding is a bounded list at its maximum (the schema's grammar is unrolled; libcbv2g loops), empty strings (EXI's second-level end event, which ISO 15118 needs decoders to handle with its non-strict schema-informed coding: V2G-DC-487, V2G20-099, V2G20-2310) and serial numbers above libcbv2g's 29 EXI octets (RFC 5280 allows 20 octets of serial, so only the oversized test case fails).
- **libcbv2g is the code of its master (03350be); the generator moves on.** Regenerated with cbexigen `main` (22efcc9) the same 12 documents fail. With the open pull request #145 of cbexigen (breakout of full arrays, see its issue #143) the two ISO 15118-20 documents with full lists round-trip (74 of 80) and the DIN `PaymentOptions` decode correctly. The rest are empty strings (second-level end events, not supported) and an oversized serial number.
- **Not compared**: robustness against malformed input, streaming, EXI features beyond the schema-informed profile ISO 15118 uses, encoding speed from the application's own data model (each library has its own).

## What the first run showed, and what it led to

The first run had this crate behind OpenV2G on decoding (173 against 96 ns for DIN). A profile (`sample` on `rust/src/bin/spin.rs`, which decodes the DIN corpus in a loop) found:

| cost | cause | change | effect on DIN decode |
|---|---|---|---|
| a quarter to a third of the time in `memmove` | a message is a few bytes, so most bit reads were within eight bytes of the end of the buffer and took a path that copied the tail with a variable-length `copy_from_slice`; the writer's flush and the string table's clone did the same | read and write those octets one by one (`bits.rs`, `strings.rs`) | 215 to 136 ns (the spin loop) |
| event codes worked out for every event | the width of an event code and the productions after the current element were derived from the whole slot table each time | the widths and the following slots are computed once per slot when the layout is built (`layout.rs`, `fill`) | 215 to 183 |
| lists decoded twice | the arena slice of a list has to have its exact size, so every list was parsed once to count it (with a copy of the string table) and once for real | a list of up to four entries is decoded once into a buffer on the stack and copied to the arena; longer lists rewind and count as before (`codec.rs`, `Dec::list`) | 129 to 109 |

Together: DIN decode 173 to 99 ns, encode 92 to 76; ISO 15118-2 190/109 to 117/88; ISO 15118-20 217/124 to 144/95. The price is code size, 7% more (the list buffer is generated per entry type): 169 to 181 kB for DIN.

**Why OpenV2G is fast, and what is left.** Its codec is generated code with the event code widths as constants, decoding straight into one static struct: no interpretation of a layout, no value moves, no arena, and lists are fixed-size arrays, so one pass. That costs a 50 kB struct for every document and the limits of fixed sizes (the failures in the round-trip table). What this crate still does per event is the table lookup in `Layout::read` (about half of the decode time now) and moving whole values out of `decode_content` (a message is 100 to 260 bytes). Further steps, not done: generating the state machine with constant widths per type instead of looking the widths up; making `Event` and `Pos` smaller so that `read` returns in registers; decoding into the final place instead of returning large values; a bit reader that keeps a window of the stream instead of loading eight octets per read.

## Layout

| | |
|---|---|
| `corpus.py`, `corpus.txt` | the documents |
| `rust/` | harness for this crate (`exi-bench`, a workspace member; features `din`, `iso2`, `iso20` select the codec for the size builds) |
| `c/` | harnesses for OpenV2G and libcbv2g, `build.sh` |
| `java/Bench.java` | harness for EXIficient |
| `run.sh`, `report.py` | run everything and write `RESULTS.md` |

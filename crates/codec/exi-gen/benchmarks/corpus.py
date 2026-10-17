#!/usr/bin/env python3
"""Writes corpus.txt (name, group, hex, schema) from the reference vectors of `cargo xtask vectors`:
the documents EXIficient made, which every codec under test has to decode and encode again."""
import pathlib, sys
here = pathlib.Path(__file__).resolve().parent
vectors = here / "../../../../data/vectors"
rows = []
for f in sorted(vectors.glob("*.hex")):
    n = f.stem
    if n.startswith(("app_protocol", "header_", "fragment")):
        continue
    group = "din" if n.startswith("din_") else "iso2" if n.startswith("iso2_") else "iso20"
    # The schema of an ISO 15118-20 document is told by the payload type on the wire, not by the stream.
    schema = group if group != "iso20" else "dc" if n.startswith("dc_") else "ac" if n.startswith("ac_") else "common"
    rows.append(f"{n}\t{group}\t{f.read_text().strip()}\t{schema}")
out = here / "corpus.txt"
out.write_text("\n".join(rows) + "\n")
print(f"{len(rows)} documents -> {out}", file=sys.stderr)

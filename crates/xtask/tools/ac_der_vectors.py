#!/usr/bin/env python3
"""Makes the XML inputs of the AC DER reference vectors (crates/xtask/vectors/acder_*.xml) and their manifest lines.

The AC DER schemas add members to the substitution groups of the AC messages (payload types 0x8010, 0x8011), so
the messages are the AC ones. For each message and each member of its substitution group this writes two valid
documents: with only the mandatory elements and with every optional one. EXIficient encodes them
(`cargo xtask vectors`); `tests/ac_der_vectors.rs` decodes and encodes them again.

Usage (from the repository root, schemas in data/schemas/iso15118-20): python3 crates/xtask/tools/ac_der_vectors.py
"""
import re
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

XS = "http://www.w3.org/2001/XMLSchema"
SCHEMAS = Path("data/schemas/iso15118-20")
OUT = Path("crates/xtask/vectors")
CT = "urn:iso:std:iso:15118:-20:CommonTypes"
AC = "urn:iso:std:iso:15118:-20:AC"


class World:
    def __init__(self, files):
        self.complex, self.simple, self.elements, self.form = {}, {}, {}, {}
        for f in files:
            self.load(SCHEMAS / f)

    def load(self, path):
        prefixes, root = {}, None
        for event, node in ET.iterparse(path, events=("start-ns", "start")):
            if event == "start-ns":
                prefixes[node[0]] = node[1]
            elif root is None:
                root = node
        ns = root.get("targetNamespace")

        def qn(value):
            if ":" in value:
                p, local = value.split(":", 1)
                return (prefixes[p], local)
            return (prefixes.get("", ns) if value not in ("string",) else XS, value)

        def tag(n):
            return n.tag.split("}")[1]

        def items(parent):
            out = []
            for c in parent:
                t = tag(c)
                lo, hi = int(c.get("minOccurs", "1")), c.get("maxOccurs", "1")
                hi = 10**9 if hi == "unbounded" else int(hi)
                if t == "element":
                    if c.get("ref"):
                        out.append(("ref", qn(c.get("ref")), lo, hi, ns))
                    else:
                        out.append(("el", (ns, c.get("name")), qn(c.get("type")), lo, hi))
                elif t == "choice":
                    out.append(("choice", items(c), lo, hi))
                elif t == "sequence":
                    out.extend(items(c))
            return out

        for c in root:
            t = tag(c)
            if t == "complexType":
                d = {"ns": ns, "base": None, "items": [], "abstract": c.get("abstract") == "true", "simple": None}
                for k in c:
                    kt = tag(k)
                    if kt in ("sequence", "choice"):
                        d["items"] = items(c if kt == "choice" and False else [k] if kt == "choice" else k)
                    elif kt == "complexContent":
                        ext = k[0]
                        d["base"] = qn(ext.get("base"))
                        for e in ext:
                            if tag(e) in ("sequence", "choice"):
                                d["items"] = items([e] if tag(e) == "choice" else e)
                    elif kt == "simpleContent":
                        d["simple"] = qn(k[0].get("base"))
                self.complex[(ns, c.get("name"))] = d
            elif t == "simpleType":
                r = c[0]
                d = {"base": qn(r.get("base")), "enum": [], "min": None, "max": None, "maxlen": None, "len": None}
                for k in r:
                    kt, v = tag(k), k.get("value")
                    if kt == "enumeration":
                        d["enum"].append(v)
                    elif kt == "minInclusive":
                        d["min"] = int(v)
                    elif kt == "maxInclusive":
                        d["max"] = int(v)
                    elif kt == "maxLength":
                        d["maxlen"] = int(v)
                    elif kt == "length":
                        d["len"] = int(v)
                self.simple[(ns, c.get("name"))] = d
            elif t == "element":
                self.elements[(ns, c.get("name"))] = {
                    "type": qn(c.get("type")) if c.get("type") else None,
                    "group": qn(c.get("substitutionGroup")) if c.get("substitutionGroup") else None,
                    "abstract": c.get("abstract") == "true",
                }

    def members(self, head):
        out, todo = [], [head]
        while todo:
            h = todo.pop(0)
            if h in out:
                continue
            out.append(h)
            todo += [n for n, e in self.elements.items() if e["group"] == h]
        def abstract(m):
            e = self.elements[m]
            return e["abstract"] or (e["type"] in self.complex and self.complex[e["type"]]["abstract"])

        return [m for m in out if not abstract(m)]

    def chain(self, qn):
        out = []
        while qn:
            d = self.complex[qn]
            out.insert(0, (qn, d))
            qn = d["base"]
        return out


class Writer:
    def __init__(self, world, mode, pick):
        self.w, self.mode, self.pick, self.n = world, mode, pick, 0
        self.prefix = {}

    def p(self, ns):
        return self.prefix.setdefault(ns, {CT: "ct", AC: "ac"}.get(ns, "x%d" % len(self.prefix)))

    def value(self, qn):
        self.n += 1
        if qn[0] == XS:
            t = qn[1]
            return {"boolean": ["true", "false"][self.n % 2], "unsignedByte": str(3 + self.n % 5), "byte": str(self.n % 7), "short": str(-5 + self.n),
                    "unsignedShort": str(100 + self.n), "int": str(self.n), "unsignedInt": str(1000 + self.n), "long": str(self.n),
                    "unsignedLong": str(1700000000 + self.n), "string": "t" + str(self.n), "hexBinary": "0102030405060708", "base64Binary": "AQIDBA=="}[t]
        d = self.w.simple[qn]
        if d["enum"]:
            return d["enum"][(self.n - 1) % len(d["enum"])] if "OK" not in d["enum"] else "OK"
        base = d["base"]
        if base[0] == XS and base[1] in ("hexBinary", "base64Binary"):
            n = d["len"] or min(d["maxlen"] or 4, 4)
            raw = bytes(range(1, n + 1))
            return raw.hex().upper() if base[1] == "hexBinary" else __import__("base64").b64encode(raw).decode()
        if base[0] == XS and base[1] == "string":
            return "s" * min(d["maxlen"] or 6, 6)
        if d["min"] is not None or d["max"] is not None:
            lo, hi = d["min"], d["max"]
            lo = lo if lo is not None else (-100 if hi is None or hi >= -100 else hi)
            v = max(lo, min(hi if hi is not None else lo + 50, lo + 5 + self.n % 3))
            return str(v)
        return self.value(base)

    def element(self, name_qn, type_qn, depth):
        tag = f"{self.p(name_qn[0])}:{name_qn[1]}"
        if type_qn[0] == XS or type_qn in self.w.simple:
            return f"<{tag}>{self.value(type_qn)}</{tag}>"
        return f"<{tag}>{self.body(type_qn, depth + 1)}</{tag}>"

    def body(self, type_qn, depth=0):
        d0 = self.w.complex[type_qn]
        if d0["simple"]:
            return self.value(d0["simple"])
        out = []
        for qn, d in self.w.chain(type_qn):
            out.extend(self.items(d["items"], depth))
        return "".join(out)

    def items(self, items, depth):
        out = []
        for it in items:
            kind = it[0]
            if kind == "choice":
                alts = it[1]
                alt = alts[0] if self.mode == "min" else alts[-1]
                out.extend(self.items([alt], depth))
                continue
            if kind == "ref" and it[1][0] == "http://www.w3.org/2000/09/xmldsig#":
                continue
            lo, hi = (it[2], it[3]) if kind == "ref" else (it[3], it[4])
            if lo == 0 and self.mode == "min":
                continue
            # At least minOccurs copies (EXIficient encodes fewer without a word, as a document no decoder accepts).
            count = max(lo, 1) if self.mode == "min" else min(hi, max(lo, 2))
            for _ in range(count):
                if kind == "ref":
                    head = it[1]
                    members = self.w.members(head)
                    chosen = self.pick.get(head, members[0])
                    out.append(self.element(chosen, self.w.elements[chosen]["type"], depth))
                else:
                    name, ty = it[1], it[2]
                    if ty[0] == "http://www.w3.org/2000/09/xmldsig#" or name[1] == "Signature":
                        continue
                    out.append(self.element(name, ty, depth))
        return "".join(out)


def snake(s):
    return re.sub(r"(?<=[a-z0-9])(?=[A-Z])", "_", s).replace("__", "_").lower()


def main():
    manifest = (OUT / "manifest.txt").read_text().splitlines()
    have = {l.split()[0] for l in manifest if l and not l.startswith("#")}
    new = []
    for variant, der_ns in (("iec", "urn:iso:std:iso:15118:-20:AC-DER-IEC"), ("sae", "urn:iso:std:iso:15118:-20:AC-DER-SAE")):
        xsd = f"V2G_CI_AC_DER_{variant.upper()}.xsd"
        world = World(["V2G_CI_CommonTypes.xsd", "V2G_CI_AC.xsd", xsd])
        roots = [(n, e) for n, e in world.elements.items() if n[0] == AC and e["type"] and e["type"][1].endswith(("ReqType", "ResType"))]
        for (ns, rname), el in sorted(roots):
            # The substitution reference of the message (energy transfer mode or control mode).
            heads = []
            for qn, d in world.chain(el["type"]):
                heads += [it[1] for it in d["items"] if it[0] == "ref" and len(world.members(it[1])) > 1]
            assert len(heads) == 1, (rname, heads)
            for member in world.members(heads[0]):
                for mode in ("min", "max"):
                    w = Writer(world, mode, {heads[0]: member})
                    body = w.body(el["type"])
                    w.prefix.setdefault(CT, "ct"); w.prefix.setdefault(AC, "ac"); w.prefix.setdefault(der_ns, "der")
                    decl = "".join(f' xmlns:{p}="{n}"' for n, p in w.prefix.items())
                    # The prefix of the DER namespace may have been named x1 while writing: rename it.
                    xml = f'<?xml version="1.0" encoding="UTF-8"?>\n<ac:{rname}{decl}>{body}</ac:{rname}>\n'
                    short = {"AC_ChargeParameterDiscoveryReq": "cpd_req", "AC_ChargeParameterDiscoveryRes": "cpd_res", "AC_ChargeLoopReq": "cl_req", "AC_ChargeLoopRes": "cl_res"}[rname]
                    kind = re.sub(r"_?(cl(req|res)_control_mode|cpd(req|res)_energy_transfer_mode)$", "", snake(member[1])) or "base"
                    name = f"acder_{variant}_{short}_{kind}_{mode}"
                    (OUT / f"{name}.xml").write_text(xml)
                    if name not in have:
                        new.append(f"{name} {xsd}")
    if new:
        text = "\n".join(manifest).rstrip("\n") + "\n" + "\n".join(new) + "\n"
        (OUT / "manifest.txt").write_text(text)
    print(len(new), "new manifest lines")


if __name__ == "__main__":
    sys.exit(main())

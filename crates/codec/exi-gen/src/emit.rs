//! Rust source for one schema namespace.

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::model::{Alt, AltTy, Complex, IntEnc, Model, Scalar, Slot, SlotKind};
use crate::names;
use crate::xsd::QName;

const E: &str = "::exi::codec::Enc";
const D: &str = "::exi::codec::Dec";
const MISSING: &str = "::exi::DecodeError::InvalidValue(\"required element missing\")";

/// A choice or substitution group turned into a Rust enum.
struct EnumDef {
    name: String,
    /// `(variant, alt index, type string, usable)`.
    variants: Vec<(String, usize, String)>,
    lifetime: bool,
}

/// How a slot appears in the generated struct.
enum Shape {
    /// Not representable (all alternatives unusable and the slot optional): no field.
    Omitted,
    Value { name: String, ty: String, optional: bool },
    List { name: String, elem: String, min: u32, max: u32 },
    Enum { name: String, ty: String, optional: bool },
}

pub struct Emitter<'a, 'w> {
    pub model: &'a mut Model<'w>,
    ns: String,
    out: String,
    enums: BTreeMap<String, String>,
    lt_memo: BTreeMap<QName, bool>,
    /// The group's document root `(namespace, element)`, if its messages are not each a root
    /// but parts of one document element (DIN SPEC 70121, ISO 15118-2: `V2G_Message`).
    pub document: Option<(String, String)>,
    /// The namespaces whose global elements make up the document grammar (default: this one's
    /// imports); for a document root, every namespace of the group.
    pub grammar_ns: Vec<String>,
}

fn upper_snake(s: &str) -> String {
    names::words(s).iter().map(|w| w.to_uppercase()).collect::<Vec<_>>().join("_")
}

impl<'a, 'w> Emitter<'a, 'w> {
    pub fn new(model: &'a mut Model<'w>, ns: &str) -> Self {
        Self { model, ns: ns.to_string(), out: String::new(), enums: BTreeMap::new(), lt_memo: BTreeMap::new(), document: None, grammar_ns: Vec::new() }
    }

    fn path(&self, q: &QName) -> String {
        let name = names::type_name(&q.local);
        if q.ns == self.ns {
            name
        } else {
            let module = self.model.module_of(&q.ns).unwrap_or("unknown_module");
            format!("super::{module}::{name}")
        }
    }

    // ------------------------------------------------------------ types

    fn scalar_lt(sc: &Scalar) -> bool {
        matches!(sc, Scalar::Str { .. } | Scalar::Integer) || matches!(sc, Scalar::Bin { fixed: None, .. })
    }

    fn complex_lt(&mut self, q: &QName) -> bool {
        if let Some(&v) = self.lt_memo.get(q) {
            return v;
        }
        let c = self.model.complex[q].clone();
        let mut lt = false;
        for slot in &c.slots {
            let usable: Vec<&Alt> = slot.alts.iter().filter(|a| self.model.alt_is_usable(a)).collect();
            if usable.is_empty() {
                continue;
            }
            if slot.max > 1 {
                lt = true;
            }
            for a in usable {
                lt |= self.alt_lt(a);
            }
        }
        self.lt_memo.insert(q.clone(), lt);
        lt
    }

    fn alt_lt(&mut self, a: &Alt) -> bool {
        match &a.ty {
            AltTy::Scalar(sc) => Self::scalar_lt(sc),
            AltTy::Complex(q) => self.complex_lt(&q.clone()),
            AltTy::External(_) => true,
            AltTy::Foreign => false,
        }
    }

    fn scalar_ty(&self, sc: &Scalar) -> String {
        match sc {
            Scalar::Bool => "bool".into(),
            Scalar::Int { rust, .. } => (*rust).into(),
            Scalar::Enum { ty, .. } => self.path(ty),
            Scalar::Str { .. } => "&'a str".into(),
            Scalar::Integer => "::exi::Integer<'a>".into(),
            Scalar::Bin { fixed: Some(n), .. } => format!("[u8; {n}]"),
            Scalar::Bin { .. } => "&'a [u8]".into(),
        }
    }

    fn alt_ty(&mut self, a: &Alt) -> String {
        match &a.ty {
            AltTy::Scalar(sc) => self.scalar_ty(sc),
            AltTy::Complex(q) => {
                let q = q.clone();
                let p = self.path(&q);
                if self.complex_lt(&q) { format!("{p}<'a>") } else { p }
            }
            AltTy::External(path) => format!("{path}<'a>"),
            AltTy::Foreign => "()".into(),
        }
    }

    fn usable_alts(&self, slot: &Slot) -> Vec<(usize, Alt)> {
        slot.alts.iter().cloned().enumerate().filter(|(_, a)| self.model.alt_is_usable(a)).collect()
    }

    // ------------------------------------------------------------ naming of choices

    fn choice_naming(&self, slot: &Slot) -> (String, String, Vec<String>) {
        // (field name, enum name, variant names per alt)
        let alt_words: Vec<Vec<String>> = slot.alts.iter().map(|a| names::words(&a.name.local)).collect();
        if slot.kind == SlotKind::Subst {
            let head = slot.head.as_ref().expect("substitution group head");
            let hw = names::words(&head.local);
            let variants = alt_words
                .iter()
                .map(|w| {
                    if w.len() > hw.len() && w[w.len() - hw.len()..] == hw[..] {
                        names::pascal_words(&w[..w.len() - hw.len()])
                    } else if *w == hw {
                        "Base".to_string()
                    } else {
                        names::pascal_words(w)
                    }
                })
                .collect();
            return (names::snake_words(&hw), format!("{}Choice", names::pascal_words(&hw)), variants);
        }
        let mut suffix = 0;
        'grow: loop {
            let next = suffix + 1;
            let first = &alt_words[0];
            if next > first.len() {
                break;
            }
            let cand = &first[first.len() - next..];
            for w in &alt_words {
                if w.len() <= next || w[w.len() - next..] != *cand {
                    break 'grow;
                }
            }
            suffix = next;
        }
        if suffix == 0 {
            let variants = alt_words.iter().map(|w| names::pascal_words(w)).collect();
            return ("choice".into(), "Choice".into(), variants);
        }
        let common = alt_words[0][alt_words[0].len() - suffix..].to_vec();
        let variants = alt_words.iter().map(|w| names::pascal_words(&w[..w.len() - suffix])).collect();
        (names::snake_words(&common), format!("{}Choice", names::pascal_words(&common)), variants)
    }

    fn shapes(&mut self, q: &QName) -> Vec<Shape> {
        let c = self.model.complex[q].clone();
        let mut shapes = Vec::new();
        let mut taken: Vec<String> = Vec::new();
        for slot in c.slots.iter() {
            let usable = self.usable_alts(slot);
            if usable.is_empty() {
                shapes.push(Shape::Omitted);
                continue;
            }
            let unique = |base: String, taken: &mut Vec<String>| {
                let mut n = base.clone();
                let mut i = 2;
                while taken.contains(&n) {
                    n = format!("{base}{i}");
                    i += 1;
                }
                taken.push(n.clone());
                n
            };
            match slot.kind {
                SlotKind::Attr | SlotKind::Single => {
                    let alt = &slot.alts[0];
                    let name = unique(names::snake(&alt.name.local), &mut taken);
                    let ty = self.alt_ty(alt);
                    if slot.max > 1 {
                        shapes.push(Shape::List { name, elem: ty, min: slot.min, max: slot.max });
                    } else {
                        shapes.push(Shape::Value { name, ty, optional: slot.min == 0 });
                    }
                }
                SlotKind::Choice | SlotKind::Subst => {
                    let (field, enum_name, variants) = self.choice_naming(slot);
                    let field = unique(field, &mut taken);
                    let owner_prefix = names::type_name(&q.local);
                    let enum_name = if enum_name == "Choice" { format!("{owner_prefix}Choice") } else { enum_name };
                    let mut vs = Vec::new();
                    let mut lifetime = false;
                    for (i, a) in &usable {
                        let ty = self.alt_ty(a);
                        lifetime |= self.alt_lt(a);
                        vs.push((variants[*i].clone(), *i, ty));
                    }
                    // Identical choices (same name and members) share one enum; a different
                    // choice with the same name gets its owner's name in front.
                    let mut def = EnumDef { name: enum_name.clone(), variants: vs, lifetime };
                    if self.enums.get(&def.name).is_some_and(|existing| *existing != Self::enum_code(&def)) {
                        def.name = format!("{owner_prefix}{enum_name}");
                    }
                    let enum_name = def.name.clone();
                    let enum_ty = if lifetime { format!("{enum_name}<'a>") } else { enum_name };
                    self.emit_enum(&def);
                    shapes.push(Shape::Enum { name: field, ty: enum_ty, optional: slot.min == 0 });
                }
            }
        }
        shapes
    }

    fn enum_code(def: &EnumDef) -> String {
        let lt = if def.lifetime { "<'a>" } else { "" };
        let mut s = String::new();
        let _ = writeln!(s, "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub enum {}{lt} {{", def.name);
        for (v, _, ty) in &def.variants {
            let _ = writeln!(s, "    {v}({ty}),");
        }
        s.push_str("}\n");
        s
    }

    fn emit_enum(&mut self, def: &EnumDef) {
        self.enums.insert(def.name.clone(), Self::enum_code(def));
    }

    // ------------------------------------------------------------ values

    /// `fused`: the value is an element's content, so the `CH` and `EE` codes around it are
    /// written together with it by one call (fixed-width values only, see [`Self::fusible`]).
    fn enc_scalar(&mut self, sc: &Scalar, v: &str, owner: &QName, name: &QName, fused: bool) -> Result<String, String> {
        Ok(match sc {
            Scalar::Bool if fused => format!("e.elem_bits(u32::from({v}), 1)?;"),
            Scalar::Bool => format!("e.bool({v})?;"),
            Scalar::Int { min, max, enc, narrowed, .. } => match enc {
                IntEnc::NBit { bits } => {
                    let write = if fused { "elem_bits" } else { "bits" };
                    format!(
                        "{{ let w = i128::from({v}); if w < {min} || w > {max} {{ return Err(::exi::EncodeError::InvalidValue(\"integer outside the schema's range\")); }} e.{write}((w - ({min})) as u32, {bits})?; }}"
                    )
                }
                IntEnc::Uint => {
                    let check = if *narrowed {
                        format!("if w < {min} || w > {max} {{ return Err(::exi::EncodeError::InvalidValue(\"integer outside the schema's range\")); }} ")
                    } else {
                        String::new()
                    };
                    format!("{{ let w = i128::from({v}); {check}e.uint(w as u64)?; }}")
                }
                IntEnc::Int => {
                    let check = if *narrowed {
                        format!("{{ let w = i128::from({v}); if w < {min} || w > {max} {{ return Err(::exi::EncodeError::InvalidValue(\"integer outside the schema's range\")); }} }} ")
                    } else {
                        String::new()
                    };
                    format!("{check}e.int(i64::from({v}))?;")
                }
            },
            Scalar::Enum { values, .. } => {
                let write = if fused { "elem_bits" } else { "bits" };
                format!("e.{write}({v} as u32, {})?;", exi::layout::ceil_log2(values.len() as u64))
            }
            Scalar::Str { max } => {
                let q = self.model.qid(name)?;
                let _ = owner;
                format!("e.string({q}, {v}, {})?;", if *max == usize::MAX { "usize::MAX".to_string() } else { max.to_string() })
            }
            Scalar::Integer => format!("e.integer(&{v})?;"),
            Scalar::Bin { fixed: Some(_), .. } => format!("e.binary(&{v})?;"),
            Scalar::Bin { max, .. } => {
                if *max == usize::MAX {
                    format!("e.binary({v})?;")
                } else {
                    format!("if {v}.len() > {max} {{ return Err(::exi::EncodeError::InvalidValue(\"binary value longer than the schema allows\")); }} e.binary({v})?;")
                }
            }
        })
    }

    fn dec_scalar(&mut self, sc: &Scalar, name: &QName, fused: bool) -> Result<String, String> {
        Ok(match sc {
            Scalar::Bool if fused => "(d.elem_bits(1)? == 1)".into(),
            Scalar::Bool => "d.bool()?".into(),
            Scalar::Int { rust, min, max, enc, narrowed } => match enc {
                IntEnc::NBit { bits } => {
                    let read = if fused { "elem_bits" } else { "bits" };
                    format!(
                    "{{ let w = i128::from(d.{read}({bits})?) + ({min}); if w > {max} {{ return Err(::exi::DecodeError::InvalidValue(\"integer outside the schema's range\")); }} {rust}::try_from(w).map_err(|_| ::exi::DecodeError::InvalidValue(\"integer outside the schema's range\"))? }}"
                    )
                }
                IntEnc::Uint => {
                    let check = if *narrowed {
                        format!("if i128::from(w) < {min} || i128::from(w) > {max} {{ return Err(::exi::DecodeError::InvalidValue(\"integer outside the schema's range\")); }} ")
                    } else {
                        String::new()
                    };
                    format!("{{ let w = d.uint()?; {check}{rust}::try_from(w).map_err(|_| ::exi::DecodeError::InvalidValue(\"integer outside the schema's range\"))? }}")
                }
                IntEnc::Int => {
                    let check = if *narrowed {
                        format!("if i128::from(w) < {min} || i128::from(w) > {max} {{ return Err(::exi::DecodeError::InvalidValue(\"integer outside the schema's range\")); }} ")
                    } else {
                        String::new()
                    };
                    format!("{{ let w = d.int()?; {check}{rust}::try_from(w).map_err(|_| ::exi::DecodeError::InvalidValue(\"integer outside the schema's range\"))? }}")
                }
            },
            Scalar::Enum { ty, values } => {
                let read = if fused { "elem_bits" } else { "bits" };
                format!("{}::from_index(d.{read}({})?)?", self.path(ty), exi::layout::ceil_log2(values.len() as u64))
            }
            Scalar::Str { max } => {
                let q = self.model.qid(name)?;
                format!("d.string({q}, {})?", if *max == usize::MAX { "usize::MAX".to_string() } else { max.to_string() })
            }
            Scalar::Integer => "d.integer()?".into(),
            Scalar::Bin { fixed: Some(n), .. } => format!("d.binary_fixed::<{n}>()?"),
            Scalar::Bin { max, .. } => {
                format!("d.binary({})?", if *max == usize::MAX { "usize::MAX".to_string() } else { max.to_string() })
            }
        })
    }

    /// Statements encoding the content after the start event (`v` is a by-value binding).
    fn enc_alt(&mut self, owner: &QName, alt: &Alt, v: &str, attr: bool) -> Result<String, String> {
        Ok(match &alt.ty {
            AltTy::Complex(_) | AltTy::External(_) => format!("{v}.encode_content(e)?;"),
            AltTy::Scalar(sc) => {
                let fused = !attr && Self::fusible(sc);
                let body = self.enc_scalar(sc, v, owner, &alt.name, fused)?;
                if let (false, Scalar::Str { max }) = (attr, sc) {
                    let q = self.model.qid(&alt.name)?;
                    let max = if *max == usize::MAX { "usize::MAX".to_string() } else { max.to_string() };
                    return Ok(format!("e.string_element({q}, {v}, {max})?;"));
                }
                if attr || fused { body } else { format!("e.ch()?; {body} e.ee()?;") }
            }
            AltTy::Foreign => return Err("cannot encode a foreign element".into()),
        })
    }

    /// Fixed-width values: `CH`, the value and `EE` fit one 32-bit write.
    fn fusible(sc: &Scalar) -> bool {
        match sc {
            Scalar::Bool => true,
            Scalar::Enum { values, .. } => exi::layout::ceil_log2(values.len() as u64) <= 30,
            Scalar::Int { enc: IntEnc::NBit { bits }, .. } => *bits <= 30,
            _ => false,
        }
    }

    fn dec_alt(&mut self, alt: &Alt, attr: bool) -> Result<String, String> {
        Ok(match &alt.ty {
            AltTy::Complex(q) => format!("{}::decode_content(d)?", self.path(&q.clone())),
            AltTy::External(path) => format!("{path}::decode_content(d)?"),
            AltTy::Scalar(sc) => {
                let fused = !attr && Self::fusible(sc);
                let body = self.dec_scalar(sc, &alt.name, fused)?;
                if let (false, Scalar::Str { max }) = (attr, sc) {
                    let q = self.model.qid(&alt.name)?;
                    let max = if *max == usize::MAX { "usize::MAX".to_string() } else { max.to_string() };
                    return Ok(format!("d.string_element({q}, {max})?"));
                }
                if attr || fused { body } else { format!("{{ d.expect_ch()?; let v = {body}; d.expect_ee()?; v }}") }
            }
            AltTy::Foreign => return Err("cannot decode a foreign element".into()),
        })
    }

    // ------------------------------------------------------------ complex types

    fn emit_complex(&mut self, q: &QName) -> Result<(), String> {
        let c: Complex = self.model.complex[q].clone();
        let name = names::type_name(&q.local);
        let lt = self.complex_lt(q);
        let ltp = if lt { "<'a>" } else { "" };
        let shapes = self.shapes(q);
        let layout_name = format!("L_{}", upper_snake(&name));

        let mut s = String::new();
        // layout constant
        let slots: Vec<String> = c
            .slots
            .iter()
            .map(|sl| format!("::exi::layout::Slot::new({}, {}, {})", sl.min, sl.max, sl.alts.len()))
            .collect();
        let _ = writeln!(s, "const {layout_name}: ::exi::layout::Layout = ::exi::layout::Layout(&::exi::layout::table([{}]), 0);", slots.join(", "));

        // struct
        let _ = writeln!(s, "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub struct {name}{ltp} {{");
        for sh in &shapes {
            match sh {
                Shape::Omitted => {}
                Shape::Value { name, ty, optional } => {
                    let t = if *optional { format!("Option<{ty}>") } else { ty.clone() };
                    let _ = writeln!(s, "    pub {name}: {t},");
                }
                Shape::List { name, elem, .. } => {
                    let _ = writeln!(s, "    pub {name}: &'a [{elem}],");
                }
                Shape::Enum { name, ty, optional } => {
                    let t = if *optional { format!("Option<{ty}>") } else { ty.clone() };
                    let _ = writeln!(s, "    pub {name}: {t},");
                }
            }
        }
        s.push_str("}\n");

        // impl
        let impl_head = if lt { "impl<'a> " } else { "impl " };
        let _ = writeln!(s, "{impl_head}{name}{ltp} {{");

        // encode
        let where_ = if lt { " where 'a: 'm" } else { "" };
        let self_ref = if lt { "&'m self" } else { "&self" };
        let _ = writeln!(
            s,
            "    pub fn encode_content<'m>({self_ref}, e: &mut {E}<'_, 'm>) -> Result<(), ::exi::EncodeError>{where_} {{"
        );
        s.push_str("        let mut pos = ::exi::layout::Pos::START;\n");
        for (si, (slot, sh)) in c.slots.iter().zip(&shapes).enumerate() {
            let attr = slot.kind == SlotKind::Attr;
            match sh {
                Shape::Omitted => {}
                Shape::Value { name, optional, .. } => {
                    let alt = &slot.alts[0];
                    let body = self.enc_alt(q, alt, "v", attr)?;
                    let complex = matches!(alt.ty, AltTy::Complex(_) | AltTy::External(_));
                    let r = if complex { "&" } else { "" };
                    if *optional {
                        let _ = writeln!(s, "        if let Some(v) = {r}self.{name} {{ pos = e.start(&{layout_name}, pos, {si}, 0)?; {body} }}");
                    } else {
                        let _ = writeln!(s, "        {{ let v = {r}self.{name}; pos = e.start(&{layout_name}, pos, {si}, 0)?; {body} }}");
                    }
                }
                Shape::List { name, min, max, .. } => {
                    let alt = &slot.alts[0];
                    let body = self.enc_alt(q, alt, "v", false)?;
                    let deref = if matches!(alt.ty, AltTy::Complex(_) | AltTy::External(_)) { "" } else { "let v = *v; " };
                    let lo = if *min > 0 { format!("self.{name}.len() < {min} || ") } else { String::new() };
                    let _ = writeln!(
                        s,
                        "        if {lo}self.{name}.len() > {max} {{ return Err(::exi::EncodeError::InvalidValue(\"list length outside the schema's range\")); }}\n        for v in self.{name}.iter() {{ {deref}pos = e.start(&{layout_name}, pos, {si}, 0)?; {body} }}"
                    );
                }
                Shape::Enum { name, ty, optional } => {
                    let enum_name = ty.split('<').next().unwrap_or(ty).to_string();
                    let usable = self.usable_alts(slot);
                    let (_, _, variants) = self.choice_naming(slot);
                    let mut arms = String::new();
                    for (i, a) in &usable {
                        let body = self.enc_alt(q, a, "v", false)?;
                        let deref = if matches!(a.ty, AltTy::Complex(_) | AltTy::External(_)) { "" } else { "let v = *v; " };
                        let _ = write!(arms, "{enum_name}::{}(v) => {{ {deref}pos = e.start(&{layout_name}, pos, {si}, {i})?; {body} }} ", variants[*i]);
                    }
                    if *optional {
                        let _ = writeln!(s, "        if let Some(choice) = &self.{name} {{ match choice {{ {arms} }} }}");
                    } else {
                        let _ = writeln!(s, "        match &self.{name} {{ {arms} }}");
                    }
                }
            }
        }
        let _ = writeln!(s, "        e.end(&{layout_name}, pos)\n    }}");

        // decode
        let _ = writeln!(
            s,
            "    pub fn decode_content<{}>(d: &mut {D}<'r, {}>) -> Result<Self, ::exi::DecodeError> {{",
            if lt { "'r" } else { "'r, 's" },
            if lt { "'a" } else { "'s" }
        );
        s.push_str("        let mut pos = ::exi::layout::Pos::START;\n");
        for (si, sh) in shapes.iter().enumerate() {
            match sh {
                Shape::Omitted => {}
                Shape::Value { ty, .. } | Shape::Enum { ty, .. } => {
                    let _ = writeln!(s, "        let mut f{si}: Option<{ty}> = None;");
                }
                Shape::List { elem, .. } => {
                    let _ = writeln!(s, "        let mut f{si}: Option<&'a [{elem}]> = None;");
                }
            }
        }
        s.push_str("        loop {\n            match d.event(&");
        s.push_str(&layout_name);
        s.push_str(", pos)? {\n                ::exi::layout::Event::End => break,\n                ::exi::layout::Event::Start { slot, alt, next } => match (slot, alt) {\n");
        for (si, (slot, sh)) in c.slots.iter().zip(&shapes).enumerate() {
            let attr = slot.kind == SlotKind::Attr;
            for (ai, a) in slot.alts.iter().enumerate() {
                let usable = self.model.alt_is_usable(a);
                if !usable || matches!(sh, Shape::Omitted) {
                    let why = if a.is_abstract { "abstract element" } else { "unsupported element (message signature)" };
                    let _ = writeln!(s, "                ({si}, {ai}) => return Err(::exi::DecodeError::Unsupported(\"{why}\")),");
                    continue;
                }
                match sh {
                    Shape::Omitted => {}
                    Shape::Value { .. } => {
                        let val = self.dec_alt(a, attr)?;
                        let _ = writeln!(s, "                ({si}, {ai}) => {{ f{si} = Some({val}); pos = next; }}");
                    }
                    Shape::List { .. } => {
                        let val = self.dec_alt(a, false)?;
                        let _ = writeln!(
                            s,
                            "                ({si}, {ai}) => {{ let (items, p) = d.list(&{layout_name}, {si}, next, |d| Ok({val}))?; f{si} = Some(items); pos = p; }}"
                        );
                    }
                    Shape::Enum { ty, .. } => {
                        let enum_name = ty.split('<').next().unwrap_or(ty).to_string();
                        let (_, _, variants) = self.choice_naming(slot);
                        let val = self.dec_alt(a, false)?;
                        let _ = writeln!(
                            s,
                            "                ({si}, {ai}) => {{ f{si} = Some({enum_name}::{}({val})); pos = next; }}",
                            variants[ai]
                        );
                    }
                }
            }
        }
        s.push_str("                _ => return Err(::exi::DecodeError::Unsupported(\"unexpected event\")),\n            },\n            }\n        }\n");
        s.push_str("        Ok(Self {\n");
        for (si, (slot, sh)) in c.slots.iter().zip(&shapes).enumerate() {
            match sh {
                Shape::Omitted => {}
                Shape::Value { name, optional, .. } | Shape::Enum { name, optional, .. } => {
                    if *optional {
                        let _ = writeln!(s, "            {name}: f{si},");
                    } else {
                        let _ = writeln!(s, "            {name}: f{si}.ok_or({MISSING})?,");
                    }
                }
                Shape::List { name, min, .. } => {
                    if *min > 0 {
                        let _ = writeln!(s, "            {name}: f{si}.ok_or({MISSING})?,");
                    } else {
                        let _ = writeln!(s, "            {name}: f{si}.unwrap_or(&[]),");
                    }
                }
            }
            let _ = slot;
        }
        s.push_str("        })\n    }\n}\n");
        self.out.push_str(&s);
        Ok(())
    }

    // ------------------------------------------------------------ simple enumerations

    fn emit_enumeration(&mut self, q: &QName, values: &[String]) {
        let name = names::type_name(&q.local);
        let variants: Vec<String> = values.iter().map(|v| names::pascal(v)).collect();
        let mut s = String::new();
        let _ = writeln!(s, "/// `{}`; the discriminant is the EXI code.", q.local);
        let _ = writeln!(s, "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n#[repr(u8)]\npub enum {name} {{");
        for v in &variants {
            let _ = writeln!(s, "    {v},");
        }
        let _ = writeln!(s, "}}\nimpl {name} {{\n    pub const ALL: [{name}; {}] = [", variants.len());
        for v in &variants {
            let _ = writeln!(s, "        {name}::{v},");
        }
        let _ = writeln!(
            s,
            "    ];\n    pub fn from_index(i: u32) -> Result<Self, ::exi::DecodeError> {{\n        Self::ALL.get(i as usize).copied().ok_or(::exi::DecodeError::InvalidValue(\"unknown enumeration value\"))\n    }}\n    /// The value as written in the schema.\n    pub fn xml_name(self) -> &'static str {{\n        match self {{"
        );
        for (v, raw) in variants.iter().zip(values) {
            let _ = writeln!(s, "            {name}::{v} => \"{raw}\",");
        }
        s.push_str("        }\n    }\n}\n");
        self.out.push_str(&s);
    }

    // ------------------------------------------------------------ the module

    pub fn emit_module(&mut self) -> Result<String, String> {
        let ns = self.ns.clone();
        let schema = self.model.world.schema(&ns)?;
        let file = schema.file.clone();
        let simple: Vec<(String, crate::xsd::SimpleDef)> = schema.simple.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let complex_names: Vec<QName> = schema.complex.keys().map(|k| QName::new(&ns, k)).collect();

        for (name, def) in &simple {
            if !def.enumeration.is_empty() {
                self.emit_enumeration(&QName::new(&ns, name), &def.enumeration);
            }
        }
        let mut emitted = Vec::new();
        for q in &complex_names {
            let c = &self.model.complex[q];
            if c.is_abstract || !self.model.complex_supported(q) {
                continue;
            }
            self.emit_complex(q)?;
            emitted.push(q.clone());
        }

        // Messages: global elements typed ...Req / ...Res.
        let roots = self.roots();
        let mut messages = Vec::new();
        for (index, el) in roots.iter().enumerate() {
            if el.ns != ns {
                continue;
            }
            let Some(decl) = self.model.world.element(el) else { continue };
            let Some(ty) = &decl.ty else { continue };
            if !(ty.local.ends_with("ReqType") || ty.local.ends_with("ResType")) || !emitted.contains(ty) {
                continue;
            }
            messages.push((el.clone(), ty.clone(), index));
        }
        let root_bits = exi::layout::ceil_log2(roots.len() as u64 + 1);
        let has_messages = !messages.is_empty() && self.document.is_none();
        let mut tail = String::new();
        let mut variants = String::new();
        let mut arms = String::new();
        let mut encode_arms = String::new();
        let per_message_roots = self.document.is_none();
        for (el, ty, index) in messages.iter().filter(|_| per_message_roots) {
            let tname = names::type_name(&ty.local);
            let lt = self.complex_lt(ty);
            let ltp = if lt { "<'a>" } else { "" };
            let impl_head = if lt { "impl<'a>" } else { "impl" };
            let _ = writeln!(
                tail,
                "{impl_head} {tname}{ltp} {{\n    /// Encodes the message as a complete EXI stream (root element `{}`).\n    pub fn encode(&self, out: &mut [u8]) -> Result<usize, ::exi::EncodeError> {{\n        let mut t = ::exi::strings::Table::new();\n        let mut e = {E}::new(out, &mut t)?;\n        e.root({index}, {root_bits})?;\n        self.encode_content(&mut e)?;\n        e.finish()\n    }}\n}}",
                el.local
            );
            let vname = names::pascal(&el.local);
            let _ = writeln!(variants, "    {vname}({tname}{ltp}),");
            let _ = writeln!(arms, "        {index} => Message::{vname}({tname}::decode_content(&mut d)?),");
            let _ = writeln!(encode_arms, "            Message::{vname}(m) => m.encode(out),");
        }
        if has_messages {
        let _ = writeln!(
            tail,
            "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub enum Message<'a> {{\n{variants}}}\n\n/// Decodes a complete EXI stream holding one message of this schema.\npub fn decode<'a>(buf: &[u8], arena: &'a ::exi::Arena<'_>) -> Result<Message<'a>, ::exi::DecodeError> {{\n    let mut t = ::exi::strings::Table::new();\n    let mut d = {D}::new(buf, arena, &mut t)?;\n    Ok(match d.bits({root_bits})? {{\n{arms}        _ => return Err(::exi::DecodeError::Unsupported(\"message not part of this schema or not implemented\")),\n    }})\n}}\n\nimpl Message<'_> {{\n    /// Encodes the message as a complete EXI stream.\n    pub fn encode(&self, out: &mut [u8]) -> Result<usize, ::exi::EncodeError> {{\n        match self {{\n{encode_arms}        }}\n    }}\n}}"
        );
        }

        if let Some((dns, droot)) = self.document.clone().filter(|(dns, _)| *dns == self.ns) {
            let index = roots.iter().position(|q| q.ns == dns && q.local == droot).ok_or_else(|| format!("document root {droot} not found"))?;
            let tname = names::type_name(&droot);
            let _ = writeln!(
                tail,
                "impl<'a> {tname}<'a> {{\n    /// Encodes the message as a complete EXI stream (document root `{droot}`).\n    pub fn encode(&self, out: &mut [u8]) -> Result<usize, ::exi::EncodeError> {{\n        let mut t = ::exi::strings::Table::new();\n        let mut e = {E}::new(out, &mut t)?;\n        e.root({index}, {root_bits})?;\n        self.encode_content(&mut e)?;\n        e.finish()\n    }}\n\n    /// Decodes a complete EXI stream holding one message.\n    pub fn decode(buf: &[u8], arena: &'a ::exi::Arena<'_>) -> Result<Self, ::exi::DecodeError> {{\n        let mut t = ::exi::strings::Table::new();\n        let mut d = {D}::new(buf, arena, &mut t)?;\n        if d.bits({root_bits})? != {index} {{\n            return Err(::exi::DecodeError::Unsupported(\"not a {droot} document\"));\n        }}\n        Self::decode_content(&mut d)\n    }}\n}}"
            );
        }

        let mut all = String::new();
        let _ = writeln!(
            all,
            "// @generated by exi-gen from {file}; do not edit, run `cargo xtask setup` instead.\n#![allow(clippy::all, dead_code, unused_mut, unused_variables, unused_parens, unused_braces)]\n"
        );
        for e in self.enums.values() {
            all.push_str(e);
        }
        all.push_str(&self.out);
        all.push_str(&tail);
        Ok(all)
    }

    /// All global elements of the schema and its imports sorted by local name: the
    /// document grammar's start events (the generic `SE(*)` comes last).
    pub fn roots(&self) -> Vec<QName> {
        let mut v: Vec<QName> = if self.grammar_ns.is_empty() {
            self.model.world.global_elements(&self.ns).iter().map(|e| e.name.clone()).collect()
        } else {
            let mut v: Vec<QName> = Vec::new();
            for ns in &self.grammar_ns {
                for e in self.model.world.global_elements(ns) {
                    if !v.contains(&e.name) {
                        v.push(e.name.clone());
                    }
                }
            }
            v
        };
        v.sort_by(|a, b| (&a.local, &a.ns).cmp(&(&b.local, &b.ns)));
        v
    }
}

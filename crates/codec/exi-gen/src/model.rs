//! The resolved model: for every complex type its EXI layout (slots) and the Rust shape of
//! its fields, for every simple type its EXI value representation.

use std::collections::{BTreeMap, HashMap};

use crate::names;
use crate::xsd::{self, ComplexDef, ElemParticle, Particle, QName, World, XMLDSIG, XS};

/// EXI representation of an integer type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntEnc {
    /// Range of at most 4096 values: an offset from `min` in `bits` bits.
    NBit { bits: u32 },
    Uint,
    Int,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scalar {
    Bool,
    Int { rust: &'static str, min: i128, max: i128, enc: IntEnc, narrowed: bool },
    /// An enumeration; the type is generated once under `ty`.
    Enum { ty: QName, values: Vec<String> },
    Str { max: usize },
    /// `xs:integer`: any size, `exi::Integer`.
    Integer,
    Bin { base64: bool, fixed: Option<usize>, max: usize },
}

impl Scalar {
    /// EXIficient's name for the datatype, as printed by the grammar dump tool.
    pub fn exi_name(&self) -> &'static str {
        match self {
            Scalar::Bool => "BOOLEAN",
            Scalar::Int { enc: IntEnc::NBit { .. }, .. } => "NBIT_UNSIGNED_INTEGER",
            Scalar::Int { enc: IntEnc::Uint, .. } => "UNSIGNED_INTEGER",
            Scalar::Int { enc: IntEnc::Int, .. } => "INTEGER",
            Scalar::Enum { .. } => "ENUMERATION",
            Scalar::Str { .. } => "STRING",
            Scalar::Integer => "INTEGER",
            Scalar::Bin { base64: false, .. } => "BINARY_HEX",
            Scalar::Bin { base64: true, .. } => "BINARY_BASE64",
        }
    }
}

/// Where the hand-written message signature lives.
pub const SIGNATURE_PATH: &str = "crate::signature::Signature";

#[derive(Debug, Clone)]
pub enum AltTy {
    Scalar(Scalar),
    Complex(QName),
    /// A type from a schema this generator does not model (xmldsig).
    Foreign,
    /// A hand-written type with `encode_content` and `decode_content` like a generated one
    /// (the message signature, `iso15118_20::signature::Signature`), by path.
    External(&'static str),
}

#[derive(Debug, Clone)]
pub struct Alt {
    pub name: QName,
    pub ty: AltTy,
    /// An abstract element or type: it has a production but can never appear.
    pub is_abstract: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    Attr,
    Single,
    Choice,
    /// A substitution group (an element with members).
    Subst,
}

#[derive(Debug, Clone)]
pub struct Slot {
    pub min: u32,
    pub max: u32,
    pub kind: SlotKind,
    pub alts: Vec<Alt>,
    /// For `Subst`, the head element's name.
    pub head: Option<QName>,
}

#[derive(Debug, Clone)]
pub struct Complex {
    pub qname: QName,
    pub slots: Vec<Slot>,
    pub is_abstract: bool,
}

pub struct Model<'w> {
    pub world: &'w World,
    pub complex: BTreeMap<QName, Complex>,
    /// Namespace -> Rust module name.
    pub modules: HashMap<String, String>,
    supported: HashMap<QName, bool>,
    /// String-table qname ids.
    qids: BTreeMap<QName, u8>,
}

fn builtin(local: &str) -> Option<Scalar> {
    let int = |rust, min: i128, max: i128| {
        let range = (max - min + 1) as u128;
        let enc = if range <= 4096 {
            IntEnc::NBit { bits: exi::layout::ceil_log2(range as u64) }
        } else if min >= 0 {
            IntEnc::Uint
        } else {
            IntEnc::Int
        };
        Some(Scalar::Int { rust, min, max, enc, narrowed: false })
    };
    match local {
        "boolean" => Some(Scalar::Bool),
        "integer" => Some(Scalar::Integer),
        "byte" => int("i8", -128, 127),
        "short" => int("i16", -32768, 32767),
        "int" => int("i32", i32::MIN.into(), i32::MAX.into()),
        "long" => int("i64", i64::MIN.into(), i64::MAX.into()),
        "unsignedByte" => int("u8", 0, 255),
        "unsignedShort" => int("u16", 0, 65535),
        "unsignedInt" => int("u32", 0, u32::MAX.into()),
        "unsignedLong" => int("u64", 0, u64::MAX.into()),
        "string" | "anyURI" | "ID" | "IDREF" => Some(Scalar::Str { max: usize::MAX }),
        "hexBinary" => Some(Scalar::Bin { base64: false, fixed: None, max: usize::MAX }),
        "base64Binary" => Some(Scalar::Bin { base64: true, fixed: None, max: usize::MAX }),
        _ => None,
    }
}

impl<'w> Model<'w> {
    pub fn new(world: &'w World, modules: &[(&str, &str)]) -> Result<Self, String> {
        let mut m = Model {
            world,
            complex: BTreeMap::new(),
            modules: modules.iter().map(|(ns, name)| (ns.to_string(), name.to_string())).collect(),
            supported: HashMap::new(),
            qids: BTreeMap::new(),
        };
        for (ns, _) in modules {
            let schema = world.schema(ns)?;
            for name in schema.complex.keys() {
                let q = QName::new(ns, name);
                m.resolve_complex(&q)?;
            }
        }
        let keys: Vec<QName> = m.complex.keys().cloned().collect();
        for q in keys {
            m.is_supported(&q);
        }
        Ok(m)
    }

    /// Resolves a simple type reference to its EXI value representation.
    pub fn scalar(&self, q: &QName) -> Result<Scalar, String> {
        if q.ns == XS {
            return builtin(&q.local).ok_or_else(|| format!("unsupported built-in type xs:{}", q.local));
        }
        let def = self.world.simple(q).ok_or_else(|| format!("unknown simple type {}:{}", q.ns, q.local))?;
        let base = def.base.as_ref().ok_or("simpleType without restriction")?;
        if !def.enumeration.is_empty() {
            return Ok(Scalar::Enum { ty: q.clone(), values: def.enumeration.clone() });
        }
        let mut s = self.scalar(base)?;
        match &mut s {
            Scalar::Int { min, max, enc, narrowed, .. } => {
                if let Some(lo) = def.min_inclusive {
                    *min = lo;
                    *narrowed = true;
                }
                if let Some(hi) = def.max_inclusive {
                    *max = hi;
                    *narrowed = true;
                }
                let range = (*max - *min + 1) as u128;
                *enc = if range <= 4096 {
                    IntEnc::NBit { bits: exi::layout::ceil_log2(range as u64) }
                } else if *min >= 0 {
                    IntEnc::Uint
                } else {
                    IntEnc::Int
                };
            }
            Scalar::Str { max } => {
                if let Some(n) = def.max_length {
                    *max = n as usize;
                }
            }
            Scalar::Bin { fixed, max, .. } => {
                if let Some(n) = def.length {
                    *fixed = Some(n as usize);
                    *max = n as usize;
                }
                if let Some(n) = def.max_length {
                    *max = n as usize;
                }
            }
            _ => {}
        }
        Ok(s)
    }

    fn is_simple(&self, q: &QName) -> bool {
        q.ns == XS || self.world.simple(q).is_some()
    }

    fn alt_for(&self, name: QName, ty: &QName, is_abstract: bool) -> Result<Alt, String> {
        let ty = if ty.ns == XMLDSIG && self.world.complex(ty).is_none() {
            AltTy::Foreign
        } else if self.is_simple(ty) {
            AltTy::Scalar(self.scalar(ty)?)
        } else if self.world.complex(ty).is_some() {
            AltTy::Complex(ty.clone())
        } else {
            return Err(format!("unknown type {}:{}", ty.ns, ty.local));
        };
        Ok(Alt { name, ty, is_abstract })
    }

    /// The members of a substitution group headed by `head`, sorted by local name (the
    /// head itself included, as EXIficient does).
    fn group(&self, head: &QName) -> Vec<QName> {
        let mut all = vec![head.clone()];
        let mut i = 0;
        while i < all.len() {
            let parent = all[i].clone();
            for s in self.world.schemas.values() {
                for e in &s.elements {
                    if e.substitution_group.as_ref() == Some(&parent) && !all.contains(&e.name) {
                        all.push(e.name.clone());
                    }
                }
            }
            i += 1;
        }
        all.sort_by(|a, b| (&a.local, &a.ns).cmp(&(&b.local, &b.ns)));
        all
    }

    fn element_alt(&self, name: &QName) -> Result<Alt, String> {
        let decl = self.world.element(name).ok_or_else(|| format!("unknown element {}:{}", name.ns, name.local))?;
        if name.ns == XMLDSIG {
            let ty = match name.local.as_str() {
                "Signature" => AltTy::External(SIGNATURE_PATH),
                _ => AltTy::Foreign,
            };
            return Ok(Alt { name: name.clone(), ty, is_abstract: false });
        }
        let ty = decl.ty.as_ref().ok_or_else(|| format!("element {} has no type", name.local))?;
        let abstract_type = self.world.complex(ty).is_some_and(|c| c.is_abstract);
        self.alt_for(name.clone(), ty, decl.is_abstract || abstract_type)
    }

    fn slot_for_elem(&self, p: &ElemParticle, schema: &xsd::Schema) -> Result<Slot, String> {
        if p.is_ref {
            let group = self.group(&p.name);
            let alts = group.iter().map(|n| self.element_alt(n)).collect::<Result<Vec<_>, _>>()?;
            let kind = if alts.len() > 1 { SlotKind::Subst } else { SlotKind::Single };
            return Ok(Slot { min: p.min, max: p.max, kind, alts, head: Some(p.name.clone()) });
        }
        let ty = p.ty.as_ref().ok_or("local element without a type")?;
        let abstract_type = self.world.complex(ty).is_some_and(|c| c.is_abstract);
        let ns = if schema.elements_qualified { p.name.ns.as_str() } else { "" };
        let alt = self.alt_for(QName::new(ns, &p.name.local), ty, abstract_type)?;
        Ok(Slot { min: p.min, max: p.max, kind: SlotKind::Single, alts: vec![alt], head: None })
    }

    fn collect_slots(&self, def: &ComplexDef, owner_ns: &str, out: &mut Vec<Slot>) -> Result<(), String> {
        let schema = self.world.schema(owner_ns)?;
        if def.simple_content {
            return Err("simpleContent is not supported".into());
        }
        if let Some(base) = &def.base {
            let bdef = self.world.complex(base).ok_or_else(|| format!("unknown base type {}", base.local))?;
            self.collect_slots(bdef, &base.ns, out)?;
        }
        for p in &def.particles {
            match p {
                Particle::Element(e) => {
                    out.push(self.slot_for_elem(e, schema)?);
                }
                Particle::Choice { min, max, items } => {
                    let mut alts = Vec::new();
                    for e in items {
                        if e.max != 1 || e.min != 1 || e.is_ref {
                            return Err("occurrence constraints or refs inside a choice are not supported".into());
                        }
                        alts.extend(self.slot_for_elem(e, schema)?.alts);
                    }
                    out.push(Slot { min: *min, max: *max, kind: SlotKind::Choice, alts, head: None });
                }
            }
        }
        Ok(())
    }

    fn resolve_complex(&mut self, q: &QName) -> Result<(), String> {
        let def = self.world.complex(q).ok_or_else(|| format!("unknown complex type {}", q.local))?.clone();
        let ctx = |e: String| format!("{}: {e}", q.local);
        // Attributes of the whole derivation chain, sorted by name, come first.
        let mut attrs: Vec<(QName, xsd::AttrDecl)> = Vec::new();
        let mut cur = Some((def.clone(), q.ns.clone()));
        while let Some((d, ns)) = cur {
            let schema = self.world.schema(&ns).map_err(ctx)?;
            for a in &d.attributes {
                let ans = if schema.attributes_qualified { ns.as_str() } else { "" };
                attrs.push((QName::new(ans, &a.name), a.clone()));
            }
            cur = match &d.base {
                Some(b) => Some((self.world.complex(b).ok_or_else(|| ctx("unknown base".into()))?.clone(), b.ns.clone())),
                None => None,
            };
        }
        attrs.sort_by(|a, b| (&a.0.local, &a.0.ns).cmp(&(&b.0.local, &b.0.ns)));
        let mut slots = Vec::new();
        for (qn, a) in &attrs {
            let alt = self.alt_for(qn.clone(), &a.ty, false).map_err(ctx)?;
            slots.push(Slot { min: u32::from(a.required), max: 1, kind: SlotKind::Attr, alts: vec![alt], head: None });
        }
        self.collect_slots(&def, &q.ns, &mut slots).map_err(ctx)?;
        // `collect_slots` walked the base chain too; attributes are handled above only.
        self.complex.insert(q.clone(), Complex { qname: q.clone(), slots, is_abstract: def.is_abstract });
        Ok(())
    }

    /// Can values of this complex type be encoded and decoded? A type that needs a foreign
    /// (xmldsig) element, or an abstract one, cannot.
    pub fn is_supported(&mut self, q: &QName) -> bool {
        if let Some(&s) = self.supported.get(q) {
            return s;
        }
        self.supported.insert(q.clone(), true); // cycle guard; the schemas have no recursion
        let Some(c) = self.complex.get(q).cloned() else {
            self.supported.insert(q.clone(), false);
            return false;
        };
        let mut ok = true;
        for slot in &c.slots {
            let mut usable = false;
            for alt in &slot.alts {
                usable |= self.alt_usable(alt);
            }
            if slot.min > 0 && !usable {
                ok = false;
            }
        }
        self.supported.insert(q.clone(), ok);
        ok
    }

    pub fn alt_usable(&mut self, alt: &Alt) -> bool {
        if alt.is_abstract {
            return false;
        }
        match &alt.ty {
            AltTy::Foreign => false,
            AltTy::External(_) => true,
            AltTy::Scalar(_) => true,
            AltTy::Complex(q) => self.is_supported(&q.clone()),
        }
    }

    pub fn complex_supported(&self, q: &QName) -> bool {
        self.supported.get(q).copied().unwrap_or(false)
    }

    pub fn alt_is_usable(&self, alt: &Alt) -> bool {
        !alt.is_abstract
            && match &alt.ty {
                AltTy::Foreign => false,
                AltTy::External(_) => true,
                AltTy::Scalar(_) => true,
                AltTy::Complex(q) => self.complex_supported(q),
            }
    }

    /// String-table id of a qname whose value is a string.
    pub fn qid(&mut self, q: &QName) -> Result<u8, String> {
        let next = self.qids.len();
        if let Some(&id) = self.qids.get(q) {
            return Ok(id);
        }
        let id = u8::try_from(next).map_err(|_| "too many string-valued qnames".to_string())?;
        self.qids.insert(q.clone(), id);
        Ok(id)
    }

    pub fn layout_of(&self, q: &QName) -> Vec<exi::layout::Slot> {
        let mut slots: Vec<exi::layout::Slot> = self.complex[q]
            .slots
            .iter()
            .map(|s| exi::layout::Slot::new(s.min, s.max, s.alts.len() as u16))
            .collect();
        exi::layout::fill(&mut slots);
        slots
    }

    pub fn module_of(&self, ns: &str) -> Option<&str> {
        self.modules.get(ns).map(String::as_str)
    }

    /// Words of the Rust name for a type.
    pub fn rust_type_name(&self, q: &QName) -> String {
        names::type_name(&q.local)
    }
}

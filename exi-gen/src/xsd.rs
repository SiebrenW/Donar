//! A small reader for the XSD subset the ISO 15118 schemas use.
//!
//! Unsupported constructs are reported as errors instead of being skipped, so a schema
//! revision that starts using one cannot silently produce a wrong codec.

use std::collections::BTreeMap;
use std::path::Path;

pub const XS: &str = "http://www.w3.org/2001/XMLSchema";
pub const XMLDSIG: &str = "http://www.w3.org/2000/09/xmldsig#";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QName {
    pub ns: String,
    pub local: String,
}

impl QName {
    pub fn new(ns: &str, local: &str) -> Self {
        Self { ns: ns.to_string(), local: local.to_string() }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SimpleDef {
    pub base: Option<QName>,
    pub enumeration: Vec<String>,
    pub min_inclusive: Option<i128>,
    pub max_inclusive: Option<i128>,
    pub length: Option<u64>,
    pub max_length: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct AttrDecl {
    pub name: String,
    pub ty: QName,
    pub required: bool,
}

#[derive(Debug, Clone)]
pub struct ElemParticle {
    /// Local name when declared in place, or the referenced global element's name.
    pub name: QName,
    /// The type of a local element; `None` for a `ref` (the global declaration decides).
    pub ty: Option<QName>,
    pub is_ref: bool,
    pub min: u32,
    pub max: u32,
}

#[derive(Debug, Clone)]
pub enum Particle {
    Element(ElemParticle),
    /// A choice of single elements.
    Choice { min: u32, max: u32, items: Vec<ElemParticle> },
}

#[derive(Debug, Clone, Default)]
pub struct ComplexDef {
    pub is_abstract: bool,
    pub base: Option<QName>,
    pub attributes: Vec<AttrDecl>,
    pub particles: Vec<Particle>,
    /// `simpleContent`: text with attributes (only xmldsig uses it).
    pub simple_content: bool,
}

#[derive(Debug, Clone)]
pub struct ElementDecl {
    pub name: QName,
    pub ty: Option<QName>,
    pub substitution_group: Option<QName>,
    pub is_abstract: bool,
}

#[derive(Debug, Default)]
pub struct Schema {
    pub target_ns: String,
    pub file: String,
    /// `elementFormDefault="qualified"`: local elements live in the target namespace.
    pub elements_qualified: bool,
    pub attributes_qualified: bool,
    pub simple: BTreeMap<String, SimpleDef>,
    pub complex: BTreeMap<String, ComplexDef>,
    /// Global elements in document order.
    pub elements: Vec<ElementDecl>,
    pub imports: Vec<String>,
}

#[derive(Debug, Default)]
pub struct World {
    pub schemas: BTreeMap<String, Schema>,
}

impl World {
    pub fn load(dir: &Path, files: &[&str]) -> Result<World, String> {
        let mut world = World::default();
        for file in files {
            let text = std::fs::read_to_string(dir.join(file)).map_err(|e| format!("{file}: {e}"))?;
            let schema = parse_schema(file, &text).map_err(|e| format!("{file}: {e}"))?;
            world.schemas.insert(schema.target_ns.clone(), schema);
        }
        Ok(world)
    }

    pub fn schema(&self, ns: &str) -> Result<&Schema, String> {
        self.schemas.get(ns).ok_or_else(|| format!("schema for namespace {ns} not loaded"))
    }

    pub fn complex(&self, q: &QName) -> Option<&ComplexDef> {
        self.schemas.get(&q.ns)?.complex.get(&q.local)
    }

    pub fn simple(&self, q: &QName) -> Option<&SimpleDef> {
        self.schemas.get(&q.ns)?.simple.get(&q.local)
    }

    pub fn element(&self, q: &QName) -> Option<&ElementDecl> {
        self.schemas.get(&q.ns)?.elements.iter().find(|e| e.name == *q)
    }

    /// Every global element of `ns` and of the schemas it imports, transitively.
    pub fn global_elements(&self, ns: &str) -> Vec<&ElementDecl> {
        let mut seen = vec![ns.to_string()];
        let mut i = 0;
        while i < seen.len() {
            if let Some(s) = self.schemas.get(&seen[i]) {
                for imp in &s.imports {
                    if !seen.contains(imp) {
                        seen.push(imp.clone());
                    }
                }
            }
            i += 1;
        }
        seen.iter().filter_map(|n| self.schemas.get(n)).flat_map(|s| s.elements.iter()).collect()
    }
}

fn attr_u32(node: roxmltree::Node, name: &str, default: u32) -> Result<u32, String> {
    match node.attribute(name) {
        None => Ok(default),
        Some("unbounded") => Ok(u32::MAX),
        Some(v) => v.parse().map_err(|_| format!("bad {name}={v}")),
    }
}

fn qname(node: roxmltree::Node, value: &str, target_ns: &str) -> Result<QName, String> {
    match value.split_once(':') {
        Some((prefix, local)) => {
            let ns = node.lookup_namespace_uri(Some(prefix)).ok_or_else(|| format!("unknown prefix in {value}"))?;
            Ok(QName::new(ns, local))
        }
        None => {
            // An unprefixed reference uses the default namespace (usually the target).
            let ns = node.lookup_namespace_uri(None).unwrap_or(target_ns);
            Ok(QName::new(ns, value))
        }
    }
}

fn children<'a, 'i>(node: roxmltree::Node<'a, 'i>) -> impl Iterator<Item = roxmltree::Node<'a, 'i>> {
    node.children().filter(|n| n.is_element() && n.tag_name().namespace() == Some(XS))
}

fn tag<'a>(n: roxmltree::Node<'a, '_>) -> &'a str {
    n.tag_name().name()
}

fn parse_schema(file: &str, text: &str) -> Result<Schema, String> {
    let opts = roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
    let doc = roxmltree::Document::parse_with_options(text, opts).map_err(|e| e.to_string())?;
    let root = doc.root_element();
    let target_ns = root.attribute("targetNamespace").unwrap_or("").to_string();
    let mut schema = Schema {
        target_ns: target_ns.clone(),
        file: file.to_string(),
        elements_qualified: root.attribute("elementFormDefault") == Some("qualified"),
        attributes_qualified: root.attribute("attributeFormDefault") == Some("qualified"),
        ..Default::default()
    };
    // xmldsig is only needed for its element names (they take part in the document grammar's
    // event codes); its types are not modelled.
    let foreign = target_ns == XMLDSIG;
    for node in children(root) {
        match tag(node) {
            "import" => schema.imports.push(node.attribute("namespace").unwrap_or("").to_string()),
            "simpleType" | "complexType" if foreign => {}
            "simpleType" => {
                let name = node.attribute("name").ok_or("anonymous global simpleType")?.to_string();
                schema.simple.insert(name, parse_simple(node, &target_ns)?);
            }
            "complexType" => {
                let name = node.attribute("name").ok_or("anonymous global complexType")?.to_string();
                schema.complex.insert(name, parse_complex(node, &target_ns)?);
            }
            "element" => {
                let name = node.attribute("name").ok_or("global element without a name")?;
                let mut ty = node.attribute("type").map(|t| qname(node, t, &target_ns)).transpose()?;
                // An inline complex type becomes a named type `<element>Type`.
                if let Some(inline) = children(node).find(|c| tag(*c) == "complexType") {
                    if !foreign {
                        let tname = format!("{name}Type");
                        schema.complex.insert(tname.clone(), parse_complex(inline, &target_ns)?);
                        ty = Some(QName::new(&target_ns, &tname));
                    }
                } else if ty.is_none() && !foreign {
                    return Err(format!("global element {name} has no type"));
                }
                schema.elements.push(ElementDecl {
                    name: QName::new(&target_ns, name),
                    ty,
                    substitution_group: node
                        .attribute("substitutionGroup")
                        .map(|t| qname(node, t, &target_ns))
                        .transpose()?,
                    is_abstract: node.attribute("abstract") == Some("true"),
                });
            }
            "annotation" | "attribute" | "attributeGroup" | "group" | "notation" => {}
            other => return Err(format!("unsupported top-level <{other}>")),
        }
    }
    Ok(schema)
}

fn parse_simple(node: roxmltree::Node, target_ns: &str) -> Result<SimpleDef, String> {
    let mut def = SimpleDef::default();
    for c in children(node) {
        match tag(c) {
            "restriction" => {
                def.base = Some(qname(c, c.attribute("base").ok_or("restriction without base")?, target_ns)?);
                for f in children(c) {
                    let v = f.attribute("value").unwrap_or("");
                    match tag(f) {
                        "enumeration" => def.enumeration.push(v.to_string()),
                        "minInclusive" => def.min_inclusive = Some(v.parse().map_err(|_| format!("bad {v}"))?),
                        "maxInclusive" => def.max_inclusive = Some(v.parse().map_err(|_| format!("bad {v}"))?),
                        "length" => def.length = Some(v.parse().map_err(|_| format!("bad {v}"))?),
                        "maxLength" => def.max_length = Some(v.parse().map_err(|_| format!("bad {v}"))?),
                        "annotation" => {}
                        other => return Err(format!("unsupported facet <{other}>")),
                    }
                }
            }
            "annotation" => {}
            other => return Err(format!("unsupported simpleType content <{other}>")),
        }
    }
    Ok(def)
}

fn parse_complex(node: roxmltree::Node, target_ns: &str) -> Result<ComplexDef, String> {
    let mut def = ComplexDef { is_abstract: node.attribute("abstract") == Some("true"), ..Default::default() };
    parse_content(node, target_ns, &mut def)?;
    Ok(def)
}

fn parse_content(node: roxmltree::Node, target_ns: &str, def: &mut ComplexDef) -> Result<(), String> {
    for c in children(node) {
        match tag(c) {
            "annotation" => {}
            "complexContent" => {
                for e in children(c) {
                    match tag(e) {
                        "extension" => {
                            def.base = Some(qname(e, e.attribute("base").ok_or("extension without base")?, target_ns)?);
                            parse_content(e, target_ns, def)?;
                        }
                        other => return Err(format!("unsupported complexContent <{other}>")),
                    }
                }
            }
            "simpleContent" => def.simple_content = true,
            "sequence" => {
                for p in children(c) {
                    def.particles.push(parse_particle(p, target_ns)?);
                }
            }
            "choice" => def.particles.push(parse_particle(c, target_ns)?),
            "attribute" => def.attributes.push(AttrDecl {
                name: c.attribute("name").ok_or("attribute without name")?.to_string(),
                ty: qname(c, c.attribute("type").ok_or("attribute without type")?, target_ns)?,
                required: c.attribute("use") == Some("required"),
            }),
            other => return Err(format!("unsupported complexType content <{other}>")),
        }
    }
    Ok(())
}

fn parse_elem(p: roxmltree::Node, target_ns: &str) -> Result<ElemParticle, String> {
    let (min, max) = (attr_u32(p, "minOccurs", 1)?, attr_u32(p, "maxOccurs", 1)?);
    if let Some(r) = p.attribute("ref") {
        return Ok(ElemParticle { name: qname(p, r, target_ns)?, ty: None, is_ref: true, min, max });
    }
    let name = p.attribute("name").ok_or("element without name or ref")?;
    let ty = p.attribute("type").ok_or_else(|| format!("element {name} has no type (inline types unsupported)"))?;
    Ok(ElemParticle { name: QName::new(target_ns, name), ty: Some(qname(p, ty, target_ns)?), is_ref: false, min, max })
}

fn parse_particle(p: roxmltree::Node, target_ns: &str) -> Result<Particle, String> {
    match tag(p) {
        "element" => Ok(Particle::Element(parse_elem(p, target_ns)?)),
        "choice" => {
            let mut items = Vec::new();
            for c in children(p) {
                match tag(c) {
                    "element" => items.push(parse_elem(c, target_ns)?),
                    "annotation" => {}
                    other => return Err(format!("unsupported <{other}> inside choice")),
                }
            }
            Ok(Particle::Choice { min: attr_u32(p, "minOccurs", 1)?, max: attr_u32(p, "maxOccurs", 1)?, items })
        }
        other => Err(format!("unsupported particle <{other}>")),
    }
}

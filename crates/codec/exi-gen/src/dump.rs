//! Prints the grammar the generator derives for a root element, in the canonical format of
//! `crates/xtask/java/DumpTree.java`, so it can be diffed against EXIficient's.

use std::collections::HashMap;

use exi::layout::{Layout, Pos, Prod};

use crate::model::{AltTy, Model, Scalar, SlotKind};
use crate::xsd::QName;

struct Dump<'m, 'w> {
    model: &'m Model<'w>,
    interned: HashMap<String, usize>,
    lines: Vec<String>,
    memo: HashMap<(QName, Pos), usize>,
}

fn name(q: &QName) -> String {
    if q.ns.is_empty() { q.local.clone() } else { format!("{{{}}}{}", q.ns, q.local) }
}

impl Dump<'_, '_> {
    fn intern(&mut self, sig: String) -> usize {
        if let Some(&id) = self.interned.get(&sig) {
            return id;
        }
        let id = self.lines.len();
        self.interned.insert(sig.clone(), id);
        self.lines.push(format!("S{id}: {sig}"));
        id
    }

    fn scalar_start(&mut self, sc: &Scalar) -> usize {
        let end = self.intern(String::new());
        let ee = self.intern(format!("0 EE ->S{end};"));
        self.intern(format!("0 CH:{} ->S{ee};", sc.exi_name()))
    }

    fn state(&mut self, q: &QName, pos: Pos) -> usize {
        if let Some(&id) = self.memo.get(&(q.clone(), pos)) {
            return id;
        }
        let complex = &self.model.complex[q];
        let slots = complex.slots.clone();
        let layout_slots: Vec<exi::layout::Slot> = self.model.layout_of(q);
        let layout = Layout(Box::leak(layout_slots.into_boxed_slice()), 0);
        let mut prods = Vec::new();
        layout.for_each(pos, |p| {
            prods.push(p);
            false
        });
        let mut sig = String::new();
        for (code, p) in prods.iter().enumerate() {
            match *p {
                Prod::End => {
                    let end = self.intern(String::new());
                    sig.push_str(&format!("{code} EE ->S{end};"));
                }
                Prod::Start { slot, alt } => {
                    let s = &slots[slot];
                    let a = &s.alts[alt];
                    let next_pos = Layout::after(pos, slot);
                    if s.kind == SlotKind::Attr {
                        let AltTy::Scalar(sc) = &a.ty else { panic!("attribute with a complex type") };
                        let next = self.state(q, next_pos);
                        sig.push_str(&format!("{code} AT:{}:{} ->S{next};", name(&a.name), sc.exi_name()));
                    } else {
                        let child = match &a.ty {
                            AltTy::Foreign | AltTy::External(_) => "XMLDSIG".to_string(),
                            // The message signature stays opaque, as in EXIficient's dump.
                            AltTy::Complex(c) if c.ns == crate::xsd::XMLDSIG && c.local == "SignatureType" => "XMLDSIG".to_string(),
                            AltTy::Scalar(sc) => format!("S{}", self.scalar_start(sc)),
                            AltTy::Complex(c) => format!("S{}", self.state(&c.clone(), Pos::START)),
                        };
                        let next = self.state(q, next_pos);
                        sig.push_str(&format!("{code} SE({}) child={child} ->S{next};", name(&a.name)));
                    }
                }
            }
        }
        let id = self.intern(sig);
        self.memo.insert((q.clone(), pos), id);
        id
    }
}

/// `root` is the global element's name; `roots` the sorted document-grammar element list; `entry` the namespace of the
/// schema the grammar is built from (the type of a message of an extended namespace is the one resolved for it).
pub fn dump(model: &Model, root: &QName, roots: &[QName], entry: &str) -> Result<String, String> {
    let decl = model.world.element(root).ok_or("unknown root element")?;
    let ty = decl.ty.clone().ok_or("root element without a type")?;
    let own = QName::new(entry, &ty.local);
    let ty = if ty.ns != entry && model.reentered(entry).contains(&own) { own } else { ty };
    let index = roots.iter().position(|r| r == root).ok_or("root not in the document grammar")?;
    let mut d = Dump { model, interned: HashMap::new(), lines: Vec::new(), memo: HashMap::new() };
    let start = d.state(&ty, Pos::START);
    let mut out = format!("ROOT {index} {} count={}\n", name(root), roots.len() + 1);
    for l in &d.lines {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str(&format!("START S{start}\n"));
    Ok(out)
}

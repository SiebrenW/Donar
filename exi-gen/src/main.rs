//! `exi-gen generate <group> <schema dir> <out dir>` | `exi-gen dump <group> <schema dir> <namespace> <root>`

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["generate", group, schemas, out] => {
            exi_gen::group(group).and_then(|g| exi_gen::generate(g, Path::new(schemas), Path::new(out)))
        }
        ["dump", group, schemas, ns, root] => exi_gen::group(group)
            .and_then(|g| exi_gen::dump_grammar(g, Path::new(schemas), ns, root))
            .map(|d| print!("{d}")),
        _ => Err("usage: exi-gen generate <group> <schema dir> <out dir> | dump <group> <schema dir> <namespace> <root>".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

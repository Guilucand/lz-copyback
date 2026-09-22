use object::read::archive::ArchiveFile;
use object::{Object, ObjectSymbol};

/// Parse the archive `build.rs` produced in `OUT_DIR` and assert that every global
/// definition starts with `lzcb_`, ignoring the one leading underscore Mach-O adds.
pub fn assert_all_prefixed(archive: &str) {
    let path = std::path::Path::new(env!("OUT_DIR")).join(archive);
    let data = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let archive = ArchiveFile::parse(&*data).expect("not an archive");

    let mut bad = Vec::new();
    for member in archive.members() {
        let member = member.expect("bad archive member");
        let obj = match object::File::parse(member.data(&*data).expect("member data")) {
            Ok(obj) => obj,
            Err(_) => continue, // e.g. the archive index
        };
        for sym in obj.symbols() {
            if !sym.is_definition() || !sym.is_global() {
                continue;
            }
            let name = sym.name().unwrap_or_default();
            let name = name.strip_prefix('_').unwrap_or(name);
            if !name.starts_with("lzcb_") {
                bad.push(name.to_string());
            }
        }
    }
    bad.sort();
    bad.dedup();
    assert!(
        bad.is_empty(),
        "{} exported symbol(s) missing the lzcb_ prefix; add them to lzcb_prefix.h \
         (regenerate with tools/gen-symbol-prefix.sh):\n{:#?}",
        bad.len(),
        bad
    );
}

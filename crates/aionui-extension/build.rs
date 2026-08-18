use std::env;
use std::fs;
use std::path::Path;

fn emit_rerun_paths(path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        if child.is_dir() {
            emit_rerun_paths(&child);
        } else {
            println!("cargo:rerun-if-changed={}", child.display());
        }
    }
}

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("Cargo supplies CARGO_MANIFEST_DIR");
    let builtin_skills = Path::new(&manifest_dir).join("../aionui-app/assets/builtin-skills");
    emit_rerun_paths(&builtin_skills);
}

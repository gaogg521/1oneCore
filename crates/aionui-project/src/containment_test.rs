use super::*;
use crate::canonical::canonicalize;

/// Windows `Url::to_file_path()` rejects drive-less `file:///work/proj`, so the
/// Unix literals these tests were written with never got past `canonicalize`
/// here. Containment is drive-agnostic, so prefixing one keeps the semantics.
const ROOT_PREFIX: &str = if cfg!(windows) { "file:///C:" } else { "file://" };

fn root_uri() -> String {
    format!("{ROOT_PREFIX}/work/proj")
}

/// Absolute filesystem path matching `root_uri()`, for `absolute_path` asserts.
fn root_fs_path() -> String {
    if cfg!(windows) {
        r"C:\work\proj".to_owned()
    } else {
        "/work/proj".to_owned()
    }
}

fn root() -> Canonical {
    canonicalize(&root_uri()).unwrap()
}

#[test]
fn resolves_normal_child_path() {
    let r = resolve_relative(&root(), "src/main.rs", FileOp::Read).unwrap();
    assert_eq!(r.relative_path, "src/main.rs");
    assert_eq!(r.resource_uri, format!("{}/work/proj/src/main.rs", ROOT_PREFIX));
    // The root contributes native separators; the relative part is joined as a
    // single `/`-delimited segment, so only the seam uses MAIN_SEPARATOR.
    assert_eq!(
        r.absolute_path.unwrap().to_string_lossy(),
        format!("{}{}src/main.rs", root_fs_path(), std::path::MAIN_SEPARATOR)
    );
}

#[test]
fn empty_relative_is_root_itself() {
    let r = resolve_relative(&root(), "", FileOp::Browse).unwrap();
    assert_eq!(r.relative_path, "");
    assert_eq!(r.resource_uri, root_uri());
}

#[test]
fn strips_single_dot_and_trailing_slash() {
    let r = resolve_relative(&root(), "./a/b/", FileOp::Read).unwrap();
    assert_eq!(r.relative_path, "a/b");
}

#[test]
fn interior_dot_dot_that_stays_inside_is_ok() {
    let r = resolve_relative(&root(), "a/../b", FileOp::Read).unwrap();
    assert_eq!(r.relative_path, "b");
}

#[test]
fn absolute_path_is_rejected() {
    let err = resolve_relative(&root(), "/etc/passwd", FileOp::Read).unwrap_err();
    assert_eq!(err.code(), "invalid_relative_path");
}

#[test]
fn dot_dot_escape_is_rejected() {
    assert_eq!(
        resolve_relative(&root(), "..", FileOp::Read).unwrap_err().code(),
        "invalid_relative_path"
    );
    assert_eq!(
        resolve_relative(&root(), "a/../../b", FileOp::Read).unwrap_err().code(),
        "invalid_relative_path"
    );
}

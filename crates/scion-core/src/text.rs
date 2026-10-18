/// Returns the content as text if it is UTF-8 without NUL bytes.
pub fn as_text(b: &[u8]) -> Option<&str> {
    let s = std::str::from_utf8(b).ok()?;
    if s.contains('\0') {
        None
    } else {
        Some(s)
    }
}

/// Splits into lines, keeping line terminators.
pub fn split_lines(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

/// Unified diff between two versions of `path`; `None` means absent.
pub fn unified_patch(path: &str, old: Option<&[u8]>, new: Option<&[u8]>) -> String {
    let a = if old.is_some() {
        format!("a/{path}")
    } else {
        "/dev/null".into()
    };
    let b = if new.is_some() {
        format!("b/{path}")
    } else {
        "/dev/null".into()
    };
    let mut out = format!("diff --scion a/{path} b/{path}\n");
    match (as_text(old.unwrap_or(b"")), as_text(new.unwrap_or(b""))) {
        (Some(o), Some(n)) => {
            let d = similar::TextDiff::from_lines(o, n);
            out.push_str(
                &d.unified_diff()
                    .context_radius(3)
                    .header(&a, &b)
                    .to_string(),
            );
        }
        _ => out.push_str(&format!("Binary files {a} and {b} differ\n")),
    }
    out
}

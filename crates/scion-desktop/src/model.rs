//! UI-independent view models: parsing patches and conflict text into rows.

/// Must match the `kind` codes documented on `DiffLine` in `ui/types.slint`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context = 0,
    Add = 1,
    Del = 2,
    Hunk = 3,
    Header = 4,
    Note = 5,
}

/// Syntax class of a highlighted span; must match `syn-color` in `ui/widgets.slint`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SynClass {
    Plain = 0,
    Comment = 1,
    String = 2,
    Constant = 3,
    Keyword = 4,
    Function = 5,
    Type = 6,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub class: SynClass,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedLine {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
    /// Blame annotation (changeset and author); empty for diffs.
    pub gutter: String,
    /// Syntax-highlighted pieces of `text`; empty when not highlighted.
    pub spans: Vec<Span>,
}

impl ParsedLine {
    pub fn new(kind: LineKind, text: impl Into<String>) -> Self {
        ParsedLine {
            kind,
            old_no: None,
            new_no: None,
            text: text.into(),
            gutter: String::new(),
            spans: Vec::new(),
        }
    }
}

/// A row of the side-by-side view: a pair of old/new lines, or a header,
/// hunk or note spanning both columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SplitRow {
    Full(ParsedLine),
    Pair(Option<ParsedLine>, Option<ParsedLine>),
}

/// Pairs each run of deletions with the additions that follow it, GitHub
/// style; context lines appear on both sides.
pub fn to_split(lines: &[ParsedLine]) -> Vec<SplitRow> {
    fn flush(out: &mut Vec<SplitRow>, dels: &mut Vec<ParsedLine>, adds: &mut Vec<ParsedLine>) {
        let n = dels.len().max(adds.len());
        let (mut d, mut a) = (dels.drain(..), adds.drain(..));
        for _ in 0..n {
            out.push(SplitRow::Pair(d.next(), a.next()));
        }
    }
    let mut out = Vec::new();
    let (mut dels, mut adds) = (Vec::new(), Vec::new());
    for l in lines {
        match l.kind {
            LineKind::Del => {
                if !adds.is_empty() {
                    flush(&mut out, &mut dels, &mut adds);
                }
                dels.push(l.clone());
            }
            LineKind::Add => adds.push(l.clone()),
            LineKind::Context => {
                flush(&mut out, &mut dels, &mut adds);
                out.push(SplitRow::Pair(Some(l.clone()), Some(l.clone())));
            }
            _ => {
                flush(&mut out, &mut dels, &mut adds);
                out.push(SplitRow::Full(l.clone()));
            }
        }
    }
    flush(&mut out, &mut dels, &mut adds);
    out
}

/// Rows beyond this are dropped with a note, bounding memory for huge diffs.
pub const MAX_DIFF_LINES: usize = 20_000;

fn clean(s: &str) -> String {
    s.trim_end_matches('\r').replace('\t', "    ")
}

/// `@@ -a,b +c,d @@` -> (a, c)
fn hunk_starts(line: &str) -> Option<(u32, u32)> {
    let mut parts = line.split_whitespace().skip(1);
    let start = |s: &str| s[1..].split(',').next()?.parse().ok();
    Some((start(parts.next()?)?, start(parts.next()?)?))
}

/// Parses the `diff --scion` unified patch format produced by
/// `scion_core::text::unified_patch` (possibly several files concatenated).
pub fn parse_patch(patch: &str, max: usize) -> Vec<ParsedLine> {
    let mut out = Vec::new();
    let (mut old, mut new) = (0u32, 0u32);
    let mut in_header = false;
    let total = patch.lines().count();
    for (i, line) in patch.lines().enumerate() {
        if out.len() >= max {
            out.push(ParsedLine::new(
                LineKind::Note,
                format!("… diff truncated ({} more lines)", total - i),
            ));
            break;
        }
        if let Some(rest) = line.strip_prefix("diff --scion a/") {
            let path = rest.split(" b/").next().unwrap_or(rest);
            out.push(ParsedLine::new(LineKind::Header, path));
            in_header = true;
            continue;
        }
        if in_header && (line.starts_with("--- ") || line.starts_with("+++ ")) {
            continue;
        }
        in_header = false;
        if line.starts_with("@@") {
            (old, new) = hunk_starts(line).unwrap_or((1, 1));
            out.push(ParsedLine::new(LineKind::Hunk, line));
            continue;
        }
        if line.starts_with("Binary files ") || line.starts_with('\\') {
            out.push(ParsedLine::new(LineKind::Note, line));
            continue;
        }
        let (kind, body) = match line.as_bytes().first() {
            Some(b'+') => (LineKind::Add, &line[1..]),
            Some(b'-') => (LineKind::Del, &line[1..]),
            Some(b' ') => (LineKind::Context, &line[1..]),
            _ => (LineKind::Context, line),
        };
        let mut row = ParsedLine::new(kind, clean(body));
        if kind != LineKind::Add {
            row.old_no = Some(old);
            old += 1;
        }
        if kind != LineKind::Del {
            row.new_no = Some(new);
            new += 1;
        }
        out.push(row);
    }
    out
}

/// Renders conflict-marked text: the target side as deletions, the source
/// (your changeset) side as additions.
pub fn parse_conflict(text: &str) -> Vec<ParsedLine> {
    #[derive(PartialEq)]
    enum Side {
        Outside,
        Target,
        Source,
    }
    let mut side = Side::Outside;
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with("<<<<<<< ") {
            side = Side::Target;
            out.push(ParsedLine::new(LineKind::Hunk, "▼ already landed"));
        } else if line == "=======" && side == Side::Target {
            side = Side::Source;
            out.push(ParsedLine::new(LineKind::Hunk, "▼ your changeset"));
        } else if line.starts_with(">>>>>>> ") && side == Side::Source {
            side = Side::Outside;
            out.push(ParsedLine::new(LineKind::Hunk, "▲ end of conflict"));
        } else {
            let kind = match side {
                Side::Target => LineKind::Del,
                Side::Source => LineKind::Add,
                Side::Outside => LineKind::Context,
            };
            out.push(ParsedLine::new(kind, clean(line)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use LineKind::*;

    fn kinds(rows: &[ParsedLine]) -> Vec<LineKind> {
        rows.iter().map(|r| r.kind).collect()
    }

    #[test]
    fn parses_real_patch_with_line_numbers() {
        let patch = patch("f.txt", "a\nb\nc\n", "a\nB\nc\nd\n");
        let rows = parse_patch(&patch, MAX_DIFF_LINES);
        assert_eq!(rows[0].kind, Header);
        assert_eq!(rows[0].text, "f.txt");
        assert_eq!(
            kinds(&rows[1..]),
            vec![Hunk, Context, Del, Add, Context, Add]
        );
        let b_del = &rows[3];
        assert_eq!(
            (b_del.text.as_str(), b_del.old_no, b_del.new_no),
            ("b", Some(2), None)
        );
        let d_add = &rows[6];
        assert_eq!(
            (d_add.text.as_str(), d_add.old_no, d_add.new_no),
            ("d", None, Some(4))
        );
    }

    #[test]
    fn multiple_files_and_added_file() {
        let patch = patch("a", "", "x\n") + &patch("b", "1\n", "2\n");
        let rows = parse_patch(&patch, MAX_DIFF_LINES);
        let headers: Vec<&str> = rows
            .iter()
            .filter(|r| r.kind == Header)
            .map(|r| r.text.as_str())
            .collect();
        assert_eq!(headers, ["a", "b"]);
        assert!(rows
            .iter()
            .all(|r| !r.text.starts_with("+++") && !r.text.starts_with("---")));
    }

    #[test]
    fn binary_and_truncation() {
        let rows = parse_patch(
            "diff --scion a/img b/img\nBinary files a/img and b/img differ\n",
            100,
        );
        assert_eq!(kinds(&rows), vec![Header, Note]);

        let big: String = (0..100).map(|i| format!("+{i}\n")).collect();
        let rows = parse_patch(&format!("@@ -0,0 +1,100 @@\n{big}"), 10);
        assert_eq!(rows.len(), 11);
        assert_eq!(rows[10].kind, Note);
        assert!(rows[10].text.contains("91 more lines"), "{}", rows[10].text);
    }

    #[test]
    fn deleted_line_starting_with_dashes_is_not_a_header() {
        let rows = parse_patch(
            "diff --scion a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n--- old\n+++ new\n",
            100,
        );
        assert_eq!(kinds(&rows), vec![Header, Hunk, Del, Add]);
        assert_eq!(rows[2].text, "-- old");
    }

    #[test]
    fn split_pairs_runs() {
        let rows = parse_patch(
            &patch("f", "a\nb\nc\nd\n", "a\nB\nC\nX\nd\n"),
            MAX_DIFF_LINES,
        );
        let split = to_split(&rows);
        let shape: Vec<(Option<&str>, Option<&str>)> = split
            .iter()
            .filter_map(|r| match r {
                SplitRow::Pair(l, r) => Some((
                    l.as_ref().map(|x| x.text.as_str()),
                    r.as_ref().map(|x| x.text.as_str()),
                )),
                SplitRow::Full(_) => None,
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (Some("a"), Some("a")),
                (Some("b"), Some("B")),
                (Some("c"), Some("C")),
                (None, Some("X")),
                (Some("d"), Some("d")),
            ]
        );
        assert!(matches!(split[0], SplitRow::Full(ref h) if h.kind == Header));
    }

    #[test]
    fn conflict_sides() {
        let rows = parse_conflict("a\n<<<<<<< target\nmine\n=======\nyours\n>>>>>>> source\nz\n");
        assert_eq!(
            kinds(&rows),
            vec![Context, Hunk, Del, Hunk, Add, Hunk, Context]
        );
    }

    /// A patch in the exact format the server and client produce.
    fn patch(path: &str, old: &str, new: &str) -> String {
        let old = (!old.is_empty()).then_some(old.as_bytes());
        scion_core::text::unified_patch(path, old, Some(new.as_bytes()))
    }
}

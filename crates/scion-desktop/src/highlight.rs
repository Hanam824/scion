//! Syntax highlighting for diff and blame rows using syntect's bundled
//! grammars. Scopes are reduced to a few classes so the UI can pick colors
//! that work in both light and dark mode.

use crate::model::{LineKind, ParsedLine, Span, SynClass};
use std::path::Path;
use std::sync::OnceLock;
use syntect::easy::ScopeRangeIterator;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// Larger documents are shown without highlighting to stay responsive.
pub const MAX_HIGHLIGHT_LINES: usize = 10_000;

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// Scope prefixes in priority order (first match on the innermost scope wins).
fn rules() -> &'static [(Scope, SynClass)] {
    static RULES: OnceLock<Vec<(Scope, SynClass)>> = OnceLock::new();
    RULES.get_or_init(|| {
        [
            ("comment", SynClass::Comment),
            ("string", SynClass::String),
            ("constant.numeric", SynClass::Constant),
            ("constant.language", SynClass::Constant),
            ("constant.character", SynClass::Constant),
            ("keyword", SynClass::Keyword),
            ("storage", SynClass::Keyword),
            ("entity.name.tag", SynClass::Keyword),
            ("entity.name.function", SynClass::Function),
            ("support.function", SynClass::Function),
            ("variable.function", SynClass::Function),
            ("entity.other.attribute-name", SynClass::Function),
            ("entity.name.type", SynClass::Type),
            ("entity.name.class", SynClass::Type),
            ("entity.name.struct", SynClass::Type),
            ("entity.name.enum", SynClass::Type),
            ("support.type", SynClass::Type),
            ("support.class", SynClass::Type),
            ("constant", SynClass::Constant),
        ]
        .into_iter()
        .filter_map(|(s, c)| Scope::new(s).ok().map(|s| (s, c)))
        .collect()
    })
}

fn classify(stack: &ScopeStack) -> SynClass {
    for scope in stack.as_slice().iter().rev() {
        for (prefix, class) in rules() {
            if prefix.is_prefix_of(*scope) {
                return *class;
            }
        }
    }
    SynClass::Plain
}

fn syntax_for(path: &str) -> Option<&'static SyntaxReference> {
    let ss = syntaxes();
    let p = Path::new(path);
    let by_ext = p
        .extension()
        .and_then(|e| e.to_str())
        .and_then(|e| ss.find_syntax_by_extension(e));
    let by_name = || {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| ss.find_syntax_by_extension(n))
    };
    by_ext.or_else(by_name).filter(|s| s.name != "Plain Text")
}

/// Parser state for one side (old or new) of a file.
struct Side {
    parse: ParseState,
    stack: ScopeStack,
}

impl Side {
    fn new(syntax: &SyntaxReference) -> Self {
        Side {
            parse: ParseState::new(syntax),
            stack: ScopeStack::new(),
        }
    }

    fn line(&mut self, text: &str) -> Vec<Span> {
        let line = format!("{text}\n");
        let Ok(ops) = self.parse.parse_line(&line, syntaxes()) else {
            return Vec::new();
        };
        let mut spans: Vec<Span> = Vec::new();
        for (range, op) in ScopeRangeIterator::new(&ops, &line) {
            let _ = self.stack.apply(op);
            let piece = line[range].trim_end_matches('\n');
            if piece.is_empty() {
                continue;
            }
            let class = classify(&self.stack);
            match spans.last_mut() {
                Some(last) if last.class == class => last.text.push_str(piece),
                _ => spans.push(Span {
                    text: piece.to_string(),
                    class,
                }),
            }
        }
        spans
    }
}

/// Fills `spans` for diff rows. Each file header switches grammar (by
/// path); `path` is the grammar for rows before any header (e.g. blame).
/// Context lines feed both sides so multi-line constructs stay in sync.
pub fn highlight(lines: &mut [ParsedLine], path: Option<&str>) {
    let code = lines
        .iter()
        .filter(|l| matches!(l.kind, LineKind::Add | LineKind::Del | LineKind::Context))
        .count();
    if code > MAX_HIGHLIGHT_LINES {
        return;
    }
    let mut sides = path
        .and_then(syntax_for)
        .map(|s| (Side::new(s), Side::new(s)));
    for l in lines.iter_mut() {
        match l.kind {
            LineKind::Header => sides = syntax_for(&l.text).map(|s| (Side::new(s), Side::new(s))),
            LineKind::Context => {
                if let Some((old, new)) = &mut sides {
                    old.line(&l.text);
                    l.spans = new.line(&l.text);
                }
            }
            LineKind::Add => {
                if let Some((_, new)) = &mut sides {
                    l.spans = new.line(&l.text);
                }
            }
            LineKind::Del => {
                if let Some((old, _)) = &mut sides {
                    l.spans = old.line(&l.text);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{parse_patch, MAX_DIFF_LINES};

    #[test]
    fn highlights_rust_and_skips_plain_text() {
        let patch = scion_core::text::unified_patch(
            "src/lib.rs",
            Some(b"fn a() {}\n"),
            Some(b"// comment\nfn a() { let s = \"x\"; }\n"),
        );
        let mut rows = parse_patch(&patch, MAX_DIFF_LINES);
        highlight(&mut rows, None);
        let classes = |text: &str| -> Vec<SynClass> {
            rows.iter()
                .find(|r| r.text == text)
                .unwrap()
                .spans
                .iter()
                .map(|s| s.class)
                .collect()
        };
        assert_eq!(classes("// comment"), vec![SynClass::Comment]);
        let line = classes("fn a() { let s = \"x\"; }");
        assert!(
            line.contains(&SynClass::Keyword) && line.contains(&SynClass::String),
            "{line:?}"
        );
        let joined: String = rows
            .iter()
            .find(|r| r.text.starts_with("fn a() {"))
            .unwrap()
            .spans
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            joined.starts_with("fn a()"),
            "spans cover the text: {joined}"
        );

        let mut plain = parse_patch(
            &scion_core::text::unified_patch("notes.txt", None, Some(b"hello\n")),
            MAX_DIFF_LINES,
        );
        highlight(&mut plain, None);
        assert!(plain.iter().all(|r| r.spans.is_empty()));
    }
}

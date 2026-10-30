//! Liens tests TypeScript → exigences : appels `covers("ID", ...)` dans un `test(...)` / `it(...)`.
//!
//! Analyse lexicale légère (chaînes et commentaires ignorés), sans parseur JavaScript.

use crate::requirements::{Diagnostic, Level};
use crate::scan_rust::TestLink;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Punct(char),
}

/// Découpe le source en jetons (avec leur ligne), en sautant commentaires et blancs.
fn lex(src: &str) -> Vec<(Tok, usize)> {
    let b: Vec<char> = src.chars().collect();
    let (mut i, mut line, mut out) = (0, 1, Vec::new());
    while i < b.len() {
        let c = b[i];
        if c == '\n' {
            line += 1;
            i += 1;
        } else if c.is_whitespace() {
            i += 1;
        } else if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                line += usize::from(b[i] == '\n');
                i += 1;
            }
            i += 2;
        } else if matches!(c, '"' | '\'' | '`') {
            let start_line = line;
            let mut s = String::new();
            i += 1;
            while i < b.len() && b[i] != c {
                if b[i] == '\\' && i + 1 < b.len() {
                    i += 1;
                }
                line += usize::from(b[i] == '\n');
                s.push(b[i]);
                i += 1;
            }
            i += 1;
            out.push((Tok::Str(s), start_line));
        } else if c.is_alphanumeric() || c == '_' || c == '$' {
            let mut s = String::new();
            while i < b.len()
                && (b[i].is_alphanumeric() || b[i] == '_' || b[i] == '$' || b[i] == '.')
            {
                s.push(b[i]);
                i += 1;
            }
            out.push((Tok::Ident(s), line));
        } else {
            out.push((Tok::Punct(c), line));
            i += 1;
        }
    }
    out
}

/// Index du `)` qui ferme le `(` situé en `open`.
fn closing(toks: &[(Tok, usize)], open: usize) -> usize {
    let mut depth = 0i32;
    for (k, (t, _)) in toks.iter().enumerate().skip(open) {
        match t {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => {
                depth -= 1;
                if depth == 0 {
                    return k;
                }
            }
            _ => {}
        }
    }
    toks.len()
}

fn level_of(rel: &Path) -> Option<Level> {
    let s = rel.to_string_lossy();
    let name = rel.file_name()?.to_string_lossy();
    if s.starts_with("ui/src/") && (name.ends_with(".test.ts") || name.ends_with(".test.tsx")) {
        Some(Level::Ui)
    } else if s.starts_with("ui/tests/") && name.ends_with(".spec.ts") {
        Some(Level::Ui)
    } else if s.starts_with("e2e/") && name.ends_with(".e2e.ts") {
        Some(Level::E2e)
    } else {
        None
    }
}

fn scan_file(src: &str, file: &Path, level: Level, out: &mut (Vec<TestLink>, Vec<Diagnostic>)) {
    let toks = lex(src);
    let mut k = 0;
    while k + 2 < toks.len() {
        let is_test = matches!(&toks[k].0, Tok::Ident(n) if n == "test" || n == "it")
            && toks[k + 1].0 == Tok::Punct('(');
        let Tok::Str(title) = &toks[k + 2].0 else {
            k += 1;
            continue;
        };
        if !is_test {
            k += 1;
            continue;
        }
        let end = closing(&toks, k + 1);
        let mut j = k + 3;
        while j + 1 < end {
            if matches!(&toks[j].0, Tok::Ident(n) if n == "covers")
                && toks[j + 1].0 == Tok::Punct('(')
            {
                let close = closing(&toks, j + 1);
                let line = toks[j].1;
                for (t, _) in &toks[j + 2..close] {
                    match t {
                        Tok::Str(id) => out.0.push(TestLink {
                            req_id: id.clone(),
                            test_name: title.clone(),
                            level,
                            file: file.to_path_buf(),
                            line,
                        }),
                        Tok::Punct(',') => {}
                        _ => {
                            out.1.push(Diagnostic::new(
                                file,
                                Some(line),
                                "avertissement : covers(...) avec un argument non littéral : ignoré",
                            ));
                            break;
                        }
                    }
                }
                j = close;
            }
            j += 1;
        }
        k = end;
    }
}

/// Parcourt `ui/src/**/*.test.ts(x)`, `ui/tests/**/*.spec.ts` et `e2e/**/*.e2e.ts` sous `root`.
pub fn scan_ts(root: &Path) -> (Vec<TestLink>, Vec<Diagnostic>) {
    let mut out = (Vec::new(), Vec::new());
    for top in ["ui", "e2e"] {
        let mut files: Vec<PathBuf> = walkdir::WalkDir::new(root.join(top))
            .into_iter()
            .filter_entry(|e| e.file_name() != "node_modules")
            .filter_map(Result::ok)
            .map(walkdir::DirEntry::into_path)
            .collect();
        files.sort();
        for file in files {
            let Some(level) = file.strip_prefix(root).ok().and_then(level_of) else {
                continue;
            };
            match std::fs::read_to_string(&file) {
                Ok(src) => scan_file(&src, &file, level, &mut out),
                Err(e) => out.1.push(Diagnostic::new(
                    &file,
                    None,
                    format!("avertissement : illisible : {e}"),
                )),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scan/ts")
    }

    #[test]
    fn ts_links_covers_calls_by_level() {
        let (links, _) = scan_ts(&root());
        let got: Vec<(&str, &str, Level)> = links
            .iter()
            .map(|l| (l.req_id.as_str(), l.test_name.as_str(), l.level))
            .collect();
        assert_eq!(
            got,
            vec![
                ("UI.LED.01", "affiche une coche pour ok", Level::Ui),
                ("NAV.NEXT.01", "navigue à l'image \"suivante\"", Level::Ui),
                ("NAV.KEY.01", "navigue à l'image \"suivante\"", Level::Ui),
                ("PERF.OPEN.01", "ouvre un TS de 4 Go", Level::E2e),
            ]
        );
    }

    #[test]
    fn ts_warns_on_non_literal_ids() {
        let (links, diags) = scan_ts(&root());
        assert!(!links.iter().any(|l| l.req_id == "X.01"));
        assert!(
            diags
                .iter()
                .any(|d| d.file.ends_with("open.e2e.ts") && d.line == Some(5)),
            "{diags:?}"
        );
    }

    #[test]
    fn ts_ignores_covers_in_comments() {
        let (links, _) = scan_ts(&root());
        assert!(!links.iter().any(|l| l.req_id == "FAKE.00"));
    }
}

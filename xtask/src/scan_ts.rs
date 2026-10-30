//! Liens tests TypeScript → exigences : appels `covers("ID", ...)` dans un `test(...)` / `it(...)`.
//!
//! Analyse lexicale légère, sans parseur JavaScript : chaînes, gabarits, commentaires et expressions
//! régulières sont reconnus ; une apostrophe isolée (texte JSX) est rattrapée en fin de ligne, car une
//! chaîne `'…'` ou `"…"` ne peut pas couvrir plusieurs lignes. Tout `covers` non exploitable produit un
//! avertissement, jamais un lien partiel.

use crate::requirements::{Diagnostic, Level};
use crate::scan_rust::TestLink;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    /// Chaîne littérale (y compris gabarit sans `${}`).
    Str(String),
    /// Valeur non littérale : gabarit avec `${}`, expression régulière.
    Dyn,
    Punct(char),
}

/// Une expression régulière peut commencer après ces jetons (sinon `/` est une division).
fn regex_allowed(prev: Option<&Tok>) -> bool {
    match prev {
        None => true,
        Some(Tok::Punct(c)) => "(,=:[!&|?{};+-*%<>~^".contains(*c),
        Some(Tok::Ident(k)) => matches!(k.as_str(), "return" | "typeof" | "case" | "in" | "of"),
        _ => false,
    }
}

/// Découpe le source en jetons (avec leur ligne), en sautant commentaires et blancs.
// Indices de lexer courts (b, c, i, j, s) : convention lisible pour ce type de code.
#[allow(clippy::many_single_char_names)]
fn lex(src: &str) -> Vec<(Tok, usize)> {
    let b: Vec<char> = src.chars().collect();
    let (mut i, mut line, mut out): (usize, usize, Vec<(Tok, usize)>) = (0, 1, Vec::new());
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
        } else if c == '/' && regex_allowed(out.last().map(|t| &t.0)) {
            // Expression régulière : jusqu'au `/` non échappé hors classe `[...]`, sur la même ligne.
            let (mut j, mut class) = (i + 1, false);
            while j < b.len() && b[j] != '\n' {
                match b[j] {
                    '\\' => j += 1,
                    '[' => class = true,
                    ']' => class = false,
                    '/' if !class => break,
                    _ => {}
                }
                j += 1;
            }
            if j < b.len() && b[j] == '/' {
                i = j + 1;
                while i < b.len() && b[i].is_alphabetic() {
                    i += 1; // drapeaux
                }
                out.push((Tok::Dyn, line));
            } else {
                out.push((Tok::Punct('/'), line));
                i += 1;
            }
        } else if c == '`' {
            let start_line = line;
            let (mut s, mut dynamic) = (String::new(), false);
            i += 1;
            while i < b.len() && b[i] != '`' {
                if b[i] == '\\' && i + 1 < b.len() {
                    i += 1;
                } else if b[i] == '$' && b.get(i + 1) == Some(&'{') {
                    dynamic = true;
                }
                line += usize::from(b[i] == '\n');
                s.push(b[i]);
                i += 1;
            }
            i += 1;
            out.push((if dynamic { Tok::Dyn } else { Tok::Str(s) }, start_line));
        } else if c == '"' || c == '\'' {
            // Une chaîne ne franchit pas une fin de ligne : sinon c'est une apostrophe de texte (JSX).
            let mut j = i + 1;
            let mut s = String::new();
            while j < b.len() && b[j] != c && b[j] != '\n' {
                if b[j] == '\\' && j + 1 < b.len() {
                    j += 1;
                    s.push(match b[j] {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    });
                } else {
                    s.push(b[j]);
                }
                j += 1;
            }
            if j < b.len() && b[j] == c {
                out.push((Tok::Str(s), line));
                i = j + 1;
            } else {
                out.push((Tok::Punct(c), line));
                i += 1;
            }
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
    let component =
        s.starts_with("ui/src/") && (name.ends_with(".test.ts") || name.ends_with(".test.tsx"));
    let functional = s.starts_with("ui/tests/") && name.ends_with(".spec.ts");
    if component || functional {
        Some(Level::Ui)
    } else if s.starts_with("e2e/") && name.ends_with(".e2e.ts") {
        Some(Level::E2e)
    } else {
        None
    }
}

/// Appel de test reconnu : `test`, `it` et leurs variantes `.only`, `.skip`, `.fixme`, `.todo`.
/// `.each` (titre paramétré) est reconnu pour signaler qu'il ne peut pas être tracé.
enum TestKind {
    Plain,
    Each,
}

fn test_kind(name: &str) -> Option<TestKind> {
    let (base, modifier) = name
        .split_once('.')
        .map_or((name, None), |(b, m)| (b, Some(m)));
    if base != "test" && base != "it" {
        return None;
    }
    match modifier {
        None | Some("only" | "skip" | "fixme" | "todo" | "concurrent") => Some(TestKind::Plain),
        Some("each") => Some(TestKind::Each),
        Some(_) => None,
    }
}

/// Plage de jetons d'un appel de test, avec son titre s'il est littéral.
struct TestCall {
    start: usize,
    end: usize,
    title: Option<String>,
    each: bool,
}

fn test_calls(toks: &[(Tok, usize)]) -> Vec<TestCall> {
    let mut calls = Vec::new();
    let mut k = 0;
    while k + 1 < toks.len() {
        if let Tok::Ident(n) = &toks[k].0
            && let Some(kind) = test_kind(n)
        {
            // `test.each(table)(titre, fn)` : la table, puis l'appel réel.
            let mut open = k + 1;
            if matches!(kind, TestKind::Each) && toks[open].0 == Tok::Punct('(') {
                open = closing(toks, open) + 1;
            }
            if open < toks.len() && toks[open].0 == Tok::Punct('(') {
                let end = closing(toks, open);
                let title = match toks.get(open + 1).map(|t| &t.0) {
                    Some(Tok::Str(t)) if matches!(kind, TestKind::Plain) => Some(t.clone()),
                    _ => None,
                };
                calls.push(TestCall {
                    start: open,
                    end,
                    title,
                    each: matches!(kind, TestKind::Each),
                });
            }
        }
        k += 1;
    }
    calls
}

fn scan_file(src: &str, file: &Path, level: Level, out: &mut (Vec<TestLink>, Vec<Diagnostic>)) {
    let toks = lex(src);
    let calls = test_calls(&toks);
    let warn = |out: &mut (Vec<TestLink>, Vec<Diagnostic>), line: usize, msg: &str| {
        out.1.push(Diagnostic::new(
            file,
            Some(line),
            format!("avertissement : {msg} : ignoré"),
        ));
    };
    for j in 0..toks.len().saturating_sub(1) {
        if !(matches!(&toks[j].0, Tok::Ident(n) if n == "covers")
            && toks[j + 1].0 == Tok::Punct('('))
        {
            continue;
        }
        let line = toks[j].1;
        let close = closing(&toks, j + 1);
        // Arguments : exactement une liste de chaînes littérales séparées par des virgules.
        let args = &toks[j + 2..close.min(toks.len())];
        let mut ids = Vec::new();
        let mut literal = !args.is_empty();
        for (n, (t, _)) in args.iter().enumerate() {
            match (n % 2, t) {
                (0, Tok::Str(id)) => ids.push(id.clone()),
                (1, Tok::Punct(',')) => {}
                _ => literal = false,
            }
        }
        // Appel de test le plus intérieur qui contient ce `covers`.
        let enclosing = calls
            .iter()
            .filter(|c| c.start < j && j < c.end)
            .min_by_key(|c| c.end - c.start);
        match (enclosing, literal) {
            (None, _) => warn(out, line, "covers(...) hors d'un test"),
            (Some(c), _) if c.each => warn(
                out,
                line,
                "covers(...) dans un test.each (titre paramétré, non traçable)",
            ),
            (Some(c), _) if c.title.is_none() => {
                warn(out, line, "covers(...) dans un test au titre non littéral");
            }
            (_, false) => warn(out, line, "covers(...) avec un argument non littéral"),
            (Some(c), true) => out.0.extend(ids.into_iter().map(|req_id| TestLink {
                req_id,
                test_name: c.title.clone().unwrap_or_default(),
                level,
                file: file.to_path_buf(),
                line,
            })),
        }
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
        let links: Vec<TestLink> = links
            .into_iter()
            .filter(|l| !l.file.ends_with("Hard.test.tsx"))
            .collect();
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

    fn hard() -> (Vec<TestLink>, Vec<Diagnostic>) {
        let (links, diags) = scan_ts(&root());
        let is_hard = |f: &Path| f.ends_with("Hard.test.tsx");
        (
            links.into_iter().filter(|l| is_hard(&l.file)).collect(),
            diags.into_iter().filter(|d| is_hard(&d.file)).collect(),
        )
    }

    fn ids(links: &[TestLink]) -> Vec<&str> {
        links.iter().map(|l| l.req_id.as_str()).collect()
    }

    #[test]
    fn ts_apostrophes_in_regex_or_jsx_do_not_drop_later_links() {
        let (links, _) = hard();
        assert!(ids(&links).contains(&"HARD.AFTER.01"), "{:?}", ids(&links));
        assert!(ids(&links).contains(&"HARD.JSX.01"), "{:?}", ids(&links));
    }

    #[test]
    fn ts_dynamic_ids_are_ignored_with_warning() {
        let (links, diags) = hard();
        assert!(
            !ids(&links)
                .iter()
                .any(|i| i.starts_with("NAV.") || i.starts_with("HARD.CONCAT")),
            "{:?}",
            ids(&links)
        );
        assert!(
            diags
                .iter()
                .filter(|d| d.message.contains("non littéral"))
                .count()
                >= 2,
            "{diags:?}"
        );
    }

    #[test]
    fn ts_test_modifiers_are_recognised() {
        let (links, diags) = hard();
        let got = ids(&links);
        assert!(
            got.contains(&"HARD.ONLY.01") && got.contains(&"HARD.SKIP.01"),
            "{got:?}"
        );
        assert!(!got.contains(&"HARD.EACH.01"));
        assert!(
            diags.iter().any(|d| d.message.contains("each")),
            "{diags:?}"
        );
    }

    #[test]
    fn ts_covers_outside_a_test_warns() {
        let (links, diags) = hard();
        assert!(!ids(&links).contains(&"HARD.OUTSIDE.01"));
        assert!(
            diags.iter().any(|d| d.message.contains("hors d'un test")),
            "{diags:?}"
        );
    }
}

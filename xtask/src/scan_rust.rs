//! Liens tests Rust → exigences : attribut `#[covers("ID", ...)]` sur une fonction de test.

use crate::requirements::{Diagnostic, Level};
use std::path::{Path, PathBuf};
use syn::{Attribute, Item, LitStr, Token, punctuated::Punctuated};

/// Un test qui déclare couvrir une exigence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestLink {
    pub req_id: String,
    /// Rust : `<binaire nextest>::<chemin du test>` (ex. `ma-trace::covers::adds`) ; TS : titre du test.
    pub test_name: String,
    pub level: Level,
    pub file: PathBuf,
    pub line: usize,
}

const TEST_ATTRS: [&str; 3] = ["test", "tokio::test", "rstest"];

fn attr_path(a: &Attribute) -> String {
    a.path()
        .segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

fn covers_ids(attrs: &[Attribute]) -> Option<Result<Vec<String>, syn::Error>> {
    let a = attrs
        .iter()
        .find(|a| attr_path(a).rsplit("::").next() == Some("covers"))?;
    Some(
        a.parse_args_with(Punctuated::<LitStr, Token![,]>::parse_terminated)
            .map(|ids| ids.iter().map(LitStr::value).collect()),
    )
}

/// Nom du paquet Cargo (champ `name` du Cargo.toml du crate), sinon nom du dossier.
fn package_name(crate_dir: &Path) -> String {
    std::fs::read_to_string(crate_dir.join("Cargo.toml"))
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| {
                    l.trim()
                        .strip_prefix("name")
                        .map(str::trim)
                        .and_then(|r| r.strip_prefix('='))
                })
                .map(|v| v.trim().trim_matches('"').to_owned())
        })
        .unwrap_or_else(|| {
            crate_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
}

/// (identifiant du binaire de test nextest, préfixe de module) pour un fichier d'un crate.
fn binary_and_module(crate_dir: &Path, file: &Path) -> (String, Vec<String>) {
    let pkg = package_name(crate_dir);
    let rel = file.strip_prefix(crate_dir).unwrap_or(file);
    let comps: Vec<String> = rel
        .with_extension("")
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    match comps.first().map(String::as_str) {
        Some("tests") if comps.len() >= 2 => (format!("{pkg}::{}", comps[1]), comps[2..].to_vec()),
        _ => {
            let mut m: Vec<String> = comps.into_iter().skip(1).collect();
            if matches!(m.last().map(String::as_str), Some("lib" | "main" | "mod")) {
                m.pop();
            }
            (pkg, m)
        }
    }
}

fn visit(
    items: &[Item],
    module: &mut Vec<String>,
    ctx: &Ctx<'_>,
    out: &mut (Vec<TestLink>, Vec<Diagnostic>),
) {
    for item in items {
        match item {
            Item::Fn(f) => {
                let line = f.sig.ident.span().start().line;
                match covers_ids(&f.attrs) {
                    None => {}
                    Some(Err(e)) => out.1.push(Diagnostic::new(
                        ctx.file,
                        Some(line),
                        format!("#[covers] illisible : {e}"),
                    )),
                    Some(Ok(ids)) => {
                        if !f
                            .attrs
                            .iter()
                            .any(|a| TEST_ATTRS.contains(&attr_path(a).as_str()))
                        {
                            out.1.push(Diagnostic::new(
                                ctx.file,
                                Some(line),
                                format!("avertissement : #[covers] sur `{}`, qui n'est pas un test : ignoré", f.sig.ident),
                            ));
                            continue;
                        }
                        if ctx.bin_target {
                            out.1.push(Diagnostic::new(
                                ctx.file,
                                Some(line),
                                format!(
                                    "avertissement : #[covers] sur `{}` dans une cible binaire (main.rs, bin/) : non pris en charge, placez le test dans la bibliothèque ; ignoré",
                                    f.sig.ident
                                ),
                            ));
                            continue;
                        }
                        let mut path = module.clone();
                        path.push(f.sig.ident.to_string());
                        let test_name = format!("{}::{}", ctx.binary, path.join("::"));
                        out.0.extend(ids.into_iter().map(|req_id| TestLink {
                            req_id,
                            test_name: test_name.clone(),
                            level: Level::Unit,
                            file: ctx.file.to_path_buf(),
                            line,
                        }));
                    }
                }
            }
            Item::Mod(m) => {
                if covers_ids(&m.attrs).is_some() {
                    out.1.push(Diagnostic::new(
                        ctx.file,
                        Some(m.ident.span().start().line),
                        format!(
                            "avertissement : #[covers] sur le module `{}` : ignoré",
                            m.ident
                        ),
                    ));
                }
                if let Some((_, inner)) = &m.content {
                    module.push(m.ident.to_string());
                    visit(inner, module, ctx, out);
                    module.pop();
                }
            }
            Item::Impl(i) => {
                if covers_ids(&i.attrs).is_some() {
                    out.1.push(Diagnostic::new(
                        ctx.file,
                        Some(i.impl_token.span.start().line),
                        "avertissement : #[covers] sur un bloc `impl` : ignoré",
                    ));
                }
                for it in &i.items {
                    if let syn::ImplItem::Fn(m) = it
                        && covers_ids(&m.attrs).is_some()
                    {
                        out.1.push(Diagnostic::new(
                                ctx.file,
                                Some(m.sig.ident.span().start().line),
                                format!("avertissement : #[covers] sur la méthode `{}` : ignoré (tests en fonctions libres)", m.sig.ident),
                            ));
                    }
                }
            }
            other => warn_other_item(other, ctx, out),
        }
    }
}

/// `#[covers]` sur un élément qui n'est ni une fonction, ni un module, ni un `impl` : avertissement.
fn warn_other_item(item: &Item, ctx: &Ctx<'_>, out: &mut (Vec<TestLink>, Vec<Diagnostic>)) {
    let named = match item {
        Item::Struct(x) => Some((&x.attrs, &x.ident)),
        Item::Enum(x) => Some((&x.attrs, &x.ident)),
        Item::Const(x) => Some((&x.attrs, &x.ident)),
        Item::Static(x) => Some((&x.attrs, &x.ident)),
        Item::Trait(x) => Some((&x.attrs, &x.ident)),
        Item::Type(x) => Some((&x.attrs, &x.ident)),
        _ => None,
    };
    if let Some((attrs, ident)) = named
        && covers_ids(attrs).is_some()
    {
        out.1.push(Diagnostic::new(
                            ctx.file,
                            Some(ident.span().start().line),
                            format!("avertissement : #[covers] sur l'élément `{ident}`, qui n'est pas une fonction : ignoré"),
                        ));
    }
}

struct Ctx<'a> {
    file: &'a Path,
    binary: String,
    /// `src/main.rs` ou `src/bin/**` : noms de binaires nextest non pris en charge.
    bin_target: bool,
}

/// Parcourt `crates/*/src/**/*.rs` et `crates/*/tests/**/*.rs` sous `root`.
pub fn scan_rust(root: &Path) -> (Vec<TestLink>, Vec<Diagnostic>) {
    let mut out = (Vec::new(), Vec::new());
    let Ok(crates) = std::fs::read_dir(root.join("crates")) else {
        return out;
    };
    let mut crate_dirs: Vec<PathBuf> = crates
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    crate_dirs.sort();
    for crate_dir in crate_dirs {
        for sub in ["src", "tests"] {
            let mut files: Vec<PathBuf> = walkdir::WalkDir::new(crate_dir.join(sub))
                .into_iter()
                .filter_map(Result::ok)
                .map(walkdir::DirEntry::into_path)
                .filter(|p| p.extension().is_some_and(|e| e == "rs"))
                // `tests/ui/` : entrées de trybuild (code qui ne doit pas compiler), pas des tests.
                .filter(|p| {
                    !p.strip_prefix(&crate_dir)
                        .is_ok_and(|r| r.starts_with("tests/ui"))
                })
                .collect();
            files.sort();
            for file in files {
                let parsed = std::fs::read_to_string(&file)
                    .map_err(|e| e.to_string())
                    .and_then(|t| syn::parse_file(&t).map_err(|e| e.to_string()));
                match parsed {
                    Ok(ast) => {
                        let (binary, mut module) = binary_and_module(&crate_dir, &file);
                        let rel = file.strip_prefix(&crate_dir).unwrap_or(&file);
                        let bin_target =
                            rel == Path::new("src/main.rs") || rel.starts_with("src/bin");
                        visit(
                            &ast.items,
                            &mut module,
                            &Ctx {
                                file: &file,
                                binary,
                                bin_target,
                            },
                            &mut out,
                        );
                    }
                    Err(e) => out.1.push(Diagnostic::new(
                        &file,
                        None,
                        format!("avertissement : fichier non analysable : {e}"),
                    )),
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scan/rust")
    }

    #[test]
    fn rust_links_annotated_tests() {
        let (links, _) = scan_rust(&root());
        let names: Vec<(&str, &str, usize)> = links
            .iter()
            .map(|l| (l.req_id.as_str(), l.test_name.as_str(), l.line))
            .collect();
        assert_eq!(
            names,
            vec![
                ("DEMO.ADD.01", "demo::tests::adds", 11),
                ("DEMO.ADD.02", "demo::it::integration", 5),
                ("DEMO.ADD.03", "demo::it::integration", 5),
            ]
        );
        assert!(links.iter().all(|l| l.level == Level::Unit));
    }

    #[test]
    fn rust_ignores_covers_on_non_fn_with_warning() {
        let (links, diags) = scan_rust(&root());
        assert!(!links.iter().any(|l| l.req_id == "DEMO.MOD.01"));
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("module `misplaced`") && d.line == Some(20)),
            "{diags:?}"
        );
    }

    #[test]
    fn rust_ignores_covers_without_test_attribute_with_warning() {
        let (links, diags) = scan_rust(&root());
        assert!(!links.iter().any(|l| l.req_id == "DEMO.HELP.01"));
        assert!(diags.iter().any(|d| {
            d.message
                .contains("`helper_without_test`, qui n'est pas un test")
        }));
    }

    #[test]
    fn rust_skips_trybuild_inputs() {
        let (links, diags) = scan_rust(&root());
        assert!(!links.iter().any(|l| l.req_id == "DEMO.UI.01"));
        assert!(
            !diags
                .iter()
                .any(|d| d.file.to_string_lossy().contains("tests/ui")),
            "{diags:?}"
        );
    }

    #[test]
    fn rust_warns_on_covers_on_impl_methods_and_other_items() {
        let (links, diags) = scan_rust(&root());
        for id in ["DEMO.IMPL.01", "DEMO.METH.01", "DEMO.STRUCT.01"] {
            assert!(
                !links.iter().any(|l| l.req_id == id),
                "{id} ne doit pas être lié"
            );
        }
        assert!(
            diags.iter().any(|d| d.message.contains("`impl`")),
            "{diags:?}"
        );
        assert!(
            diags.iter().any(|d| d.message.contains("méthode `method`")),
            "{diags:?}"
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("élément") && d.message.contains("Other")),
            "{diags:?}"
        );
    }

    #[test]
    fn rust_warns_on_covers_in_binary_targets() {
        let (links, diags) = scan_rust(&root());
        assert!(
            !links
                .iter()
                .any(|l| l.req_id == "DEMO.BIN.01" || l.req_id == "DEMO.MAIN.01")
        );
        let bin_warnings = diags
            .iter()
            .filter(|d| d.message.contains("cible binaire"))
            .count();
        assert_eq!(bin_warnings, 2, "{diags:?}");
    }
}

//! Vérification des catalogues Fluent (`locales/{fr,en}/engine.ftl`) contre les messages déclarés.

use std::collections::BTreeSet;
use std::path::Path;

use fluent_syntax::ast::{
    CallArguments, Entry, Expression, InlineExpression, Pattern, PatternElement,
};
use fluent_syntax::parser;
use ma_core::MsgDef;

const LANGS: [&str; 2] = ["fr", "en"];

fn vars_in_pattern(p: &Pattern<&str>, out: &mut BTreeSet<String>) {
    for el in &p.elements {
        if let PatternElement::Placeable { expression } = el {
            vars_in_expression(expression, out);
        }
    }
}

fn vars_in_args(a: &CallArguments<&str>, out: &mut BTreeSet<String>) {
    for e in &a.positional {
        vars_in_inline(e, out);
    }
    for n in &a.named {
        vars_in_inline(&n.value, out);
    }
}

fn vars_in_inline(e: &InlineExpression<&str>, out: &mut BTreeSet<String>) {
    match e {
        InlineExpression::VariableReference { id } => {
            out.insert(id.name.to_string());
        }
        InlineExpression::FunctionReference { arguments, .. } => vars_in_args(arguments, out),
        InlineExpression::TermReference {
            arguments: Some(a), ..
        } => vars_in_args(a, out),
        InlineExpression::Placeable { expression } => vars_in_expression(expression, out),
        _ => {}
    }
}

fn vars_in_expression(e: &Expression<&str>, out: &mut BTreeSet<String>) {
    match e {
        Expression::Inline(i) => vars_in_inline(i, out),
        Expression::Select { selector, variants } => {
            vars_in_inline(selector, out);
            for v in variants {
                vars_in_pattern(&v.value, out);
            }
        }
    }
}

/// Lit un catalogue : identifiant de message → variables utilisées.
fn load(path: &Path) -> Result<Vec<(String, BTreeSet<String>)>, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let res = parser::parse(src.as_str()).map_err(|(_, errs)| {
        format!(
            "{}: {} erreur(s) de syntaxe Fluent",
            path.display(),
            errs.len()
        )
    })?;
    let mut out = Vec::new();
    for entry in &res.body {
        if let Entry::Message(m) = entry {
            let mut vars = BTreeSet::new();
            if let Some(p) = &m.value {
                vars_in_pattern(p, &mut vars);
            }
            for a in &m.attributes {
                vars_in_pattern(&a.value, &mut vars);
            }
            out.push((m.id.name.to_string(), vars));
        }
    }
    Ok(out)
}

/// Compare `defs` aux catalogues `locales/<langue>/engine.ftl` : renvoie les écarts (identifiant
/// absent en `fr` ou en `en`, variables différentes de `MsgDef::params`). Vide si tout concorde.
#[must_use]
pub fn check_catalog(defs: &[MsgDef], locales: &Path) -> Vec<String> {
    let mut issues = Vec::new();
    for lang in LANGS {
        let path = locales.join(lang).join("engine.ftl");
        let catalog = match load(&path) {
            Ok(c) => c,
            Err(e) => {
                issues.push(format!("{lang}: catalogue illisible ({e})"));
                continue;
            }
        };
        for def in defs {
            let Some((_, vars)) = catalog.iter().find(|(id, _)| id == def.id) else {
                issues.push(format!("{}: identifiant absent en {lang}", def.id));
                continue;
            };
            let declared: BTreeSet<String> = def.params.iter().map(|s| (*s).to_string()).collect();
            if *vars != declared {
                issues.push(format!(
                    "{}: variables différentes en {lang} (catalogue {vars:?}, déclarées {declared:?})",
                    def.id
                ));
            }
        }
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;
    use ma_core::MsgDef;

    #[ma_trace::covers("I18N.KEYS.01")]
    #[test]
    fn catalog_detects_missing_and_mismatched_keys() {
        let dir = std::env::temp_dir().join(format!("ma-catalog-{}", std::process::id()));
        for l in ["fr", "en"] {
            std::fs::create_dir_all(dir.join(l)).unwrap();
        }
        let w = |l: &str, s: &str| std::fs::write(dir.join(l).join("engine.ftl"), s).unwrap();
        let defs = [
            MsgDef {
                id: "a-ok",
                params: &["pid"],
            },
            MsgDef {
                id: "b-miss",
                params: &[],
            },
        ];
        w("fr", "a-ok = PID { $pid }\nb-miss = x\n");
        w("en", "a-ok = PID { $pid }\n");
        let r = check_catalog(&defs, &dir);
        assert_eq!(r.len(), 1, "{r:?}");
        assert!(r[0].contains("b-miss") && r[0].contains("en"));
        w("fr", "a-ok = PID { $pid } { $extra }\nb-miss = x\n");
        w("en", "a-ok = PID { $pid }\nb-miss = y\n");
        let r = check_catalog(&defs, &dir);
        assert_eq!(r.len(), 1, "{r:?}");
        assert!(r[0].contains("a-ok") && r[0].contains("extra"));
        w("fr", "a-ok = PID { $pid }\nb-miss = x\n");
        assert_eq!(check_catalog(&defs, &dir), Vec::<String>::new());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

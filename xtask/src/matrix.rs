//! Croisement exigences × liens × résultats : matrice de traçabilité et garde de la CI.

use crate::requirements::{Diagnostic, Level, Lot, Requirement, Status};
use crate::results::{Outcome, Results};
use crate::scan_rust::TestLink;
use std::collections::HashMap;
use std::fmt::Write as _;

pub struct Coverage {
    pub req: Requirement,
    pub links: Vec<(TestLink, Outcome)>,
}

impl Coverage {
    /// Niveaux attendus sans aucun test lié qui passe.
    pub fn missing_levels(&self) -> Vec<Level> {
        self.req
            .levels
            .iter()
            .copied()
            .filter(|lvl| {
                !self
                    .links
                    .iter()
                    .any(|(l, o)| l.level == *lvl && *o == Outcome::Passed)
            })
            .collect()
    }
}

/// Applique les règles de la garde (voir le plan M0, Tâche 5) et renvoie la couverture par exigence.
pub fn evaluate(
    reqs: &[Requirement],
    links: &[TestLink],
    results: &Results,
    current: Lot,
    release: bool,
) -> (Vec<Coverage>, Vec<Diagnostic>) {
    let mut errors = Vec::new();
    let known: HashMap<&str, &Requirement> = reqs.iter().map(|r| (r.id.as_str(), r)).collect();
    for l in links {
        if !known.contains_key(l.req_id.as_str()) {
            errors.push(Diagnostic::new(
                &l.file,
                Some(l.line),
                format!(
                    "le test « {} » référence une exigence inconnue : {}",
                    l.test_name, l.req_id
                ),
            ));
        }
    }
    // Les résultats TS sont identifiés par leur titre : deux tests différents de même titre et de même
    // niveau rendraient l'appariement ambigu (l'un pourrait couvrir l'autre).
    let mut seen: HashMap<(Level, &str), &TestLink> = HashMap::new();
    for l in links.iter().filter(|l| l.level != Level::Unit) {
        if let Some(prev) = seen.insert((l.level, l.test_name.as_str()), l)
            && (&prev.file, prev.line) != (&l.file, l.line)
        {
            errors.push(Diagnostic::new(
                    &l.file,
                    Some(l.line),
                    format!(
                        "titre de test en double « {} » (niveau {}), aussi dans {}:{} : renommez l'un des tests",
                        l.test_name,
                        l.level,
                        prev.file.display(),
                        prev.line
                    ),
                ));
        }
    }
    let mut cov: Vec<Coverage> = reqs
        .iter()
        .map(|r| Coverage {
            req: r.clone(),
            links: links
                .iter()
                .filter(|l| l.req_id == r.id)
                .map(|l| {
                    (
                        l.clone(),
                        results
                            .get(&(l.level, l.test_name.clone()))
                            .copied()
                            .unwrap_or(Outcome::NotRun),
                    )
                })
                .collect(),
        })
        .collect();
    cov.sort_by(|a, b| (a.req.lot, &a.req.id).cmp(&(b.req.lot, &b.req.id)));

    for c in cov.iter().filter(|c| c.req.lot == current) {
        let r = &c.req;
        if r.status == Status::Implemented || release {
            if release && r.status != Status::Implemented {
                errors.push(Diagnostic::new(
                    &r.file,
                    Some(r.line),
                    format!("{} : non implémentée (livraison)", r.id),
                ));
            }
            let missing = c.missing_levels();
            if !missing.is_empty() {
                let names: Vec<String> = missing.iter().map(ToString::to_string).collect();
                errors.push(Diagnostic::new(
                    &r.file,
                    Some(r.line),
                    format!(
                        "{} : aucun test qui passe au(x) niveau(x) {}",
                        r.id,
                        names.join(", ")
                    ),
                ));
            }
        }
    }
    (cov, errors)
}

fn symbol(o: Outcome) -> &'static str {
    match o {
        Outcome::Passed => "✓",
        Outcome::Failed => "✗",
        Outcome::Skipped => "–",
        Outcome::NotRun => "∅",
    }
}

fn verdict(c: &Coverage) -> &'static str {
    match (c.req.status, c.missing_levels().is_empty()) {
        (Status::Implemented, true) => "✓ couverte",
        (Status::Implemented, false) => "✗ non couverte",
        (_, true) => "à faire (déjà couverte)",
        (_, false) => "à faire",
    }
}

fn lot_name(l: Lot) -> &'static str {
    match l {
        Lot::Mvp => "MVP",
        Lot::V1 => "V1",
        Lot::V2 => "V2",
        Lot::V3 => "V3",
        Lot::V4 => "V4",
        Lot::V5 => "V5",
        Lot::Later => "LATER",
    }
}

fn status_name(s: Status) -> &'static str {
    match s {
        Status::Draft => "draft",
        Status::Validated => "validated",
        Status::Implemented => "implemented",
    }
}

fn row(c: &Coverage) -> [String; 8] {
    let levels: Vec<String> = c.req.levels.iter().map(ToString::to_string).collect();
    let tests: Vec<String> = c
        .links
        .iter()
        .map(|(l, o)| format!("{} {} ({})", symbol(*o), l.test_name, l.level))
        .collect();
    [
        c.req.id.clone(),
        c.req.title.clone(),
        c.req.domain.clone(),
        lot_name(c.req.lot).to_owned(),
        status_name(c.req.status).to_owned(),
        levels.join(", "),
        tests.join("<br>"),
        verdict(c).to_owned(),
    ]
}

const HEADERS: [&str; 8] = [
    "Exigence",
    "Titre",
    "Domaine",
    "Lot",
    "Statut",
    "Niveaux",
    "Tests",
    "Couverture",
];

pub fn render_markdown(cov: &[Coverage]) -> String {
    let mut s = String::from("# Matrice de traçabilité\n\n");
    let _ = writeln!(s, "| {} |", HEADERS.join(" | "));
    let _ = writeln!(s, "|{}", "---|".repeat(HEADERS.len()));
    for c in cov {
        let cells: Vec<String> = row(c).iter().map(|v| v.replace('|', "\\|")).collect();
        let _ = writeln!(s, "| {} |", cells.join(" | "));
    }
    s
}

fn escape(v: &str) -> String {
    v.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace("&lt;br&gt;", "<br>")
}

pub fn render_html(cov: &[Coverage]) -> String {
    let mut s = String::from(
        "<!doctype html><meta charset=\"utf-8\"><title>Matrice de traçabilité</title>\
         <style>body{font:13px sans-serif}td,th{border:1px solid #ccc;padding:4px;vertical-align:top}\
         table{border-collapse:collapse}tr.detail td{color:#555;border-top:none}</style><h1>Matrice de traçabilité</h1><table><tr>",
    );
    for h in HEADERS {
        let _ = write!(s, "<th>{h}</th>");
    }
    s.push_str("</tr>");
    for c in cov {
        s.push_str("<tr>");
        for v in row(c) {
            let _ = write!(s, "<td>{}</td>", escape(&v));
        }
        s.push_str("</tr>");
        // Détail pour la relecture du référentiel : description, critères d'acceptation, source.
        let mut criteria = String::new();
        for a in &c.req.acceptance {
            let _ = write!(criteria, "<li>{}</li>", escape(a));
        }
        let _ = write!(
            s,
            "<tr class=\"detail\"><td></td><td colspan=\"{}\">{}<ul>{criteria}</ul><small>Source : {}</small></td></tr>",
            HEADERS.len() - 1,
            escape(&c.req.description),
            escape(&c.req.source),
        );
    }
    s.push_str("</table>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn req(id: &str, lot: Lot, status: Status, levels: &[Level]) -> Requirement {
        Requirement {
            id: id.into(),
            title: format!("Titre {id}"),
            description: String::new(),
            domain: "F6".into(),
            lot,
            acceptance: vec!["a".into()],
            levels: levels.to_vec(),
            source: String::new(),
            status,
            file: PathBuf::from("requirements/mvp/x.yaml"),
            line: 1,
        }
    }

    fn link(id: &str, test: &str, level: Level) -> TestLink {
        TestLink {
            req_id: id.into(),
            test_name: test.into(),
            level,
            file: PathBuf::from("t.rs"),
            line: 3,
        }
    }

    /// Résultats valables à tous les niveaux (les tests ci-dessous ne portent pas sur le niveau).
    fn results(pairs: &[(&str, Outcome)]) -> Results {
        pairs
            .iter()
            .flat_map(|(n, o)| {
                [Level::Unit, Level::Ui, Level::E2e].map(|l| ((l, (*n).to_owned()), *o))
            })
            .collect()
    }

    #[test]
    fn unknown_id_fails_gate() {
        let (_, errs) = evaluate(
            &[],
            &[link("NOPE.01", "t", Level::Unit)],
            &Results::new(),
            Lot::Mvp,
            false,
        );
        assert!(errs.iter().any(|e| e.message.contains("NOPE.01")));
    }

    #[test]
    fn implemented_requirement_needs_passing_test_per_level() {
        let r = req(
            "A.01",
            Lot::Mvp,
            Status::Implemented,
            &[Level::Unit, Level::Ui],
        );
        let l = [link("A.01", "u", Level::Unit), link("A.01", "i", Level::Ui)];
        let (_, errs) = evaluate(
            &[r],
            &l,
            &results(&[("u", Outcome::Passed), ("i", Outcome::NotRun)]),
            Lot::Mvp,
            false,
        );
        assert_eq!(errs.len(), 1);
        assert!(
            errs[0].message.contains("A.01") && errs[0].message.contains("ui"),
            "{}",
            errs[0]
        );
    }

    #[test]
    fn failed_linked_test_does_not_cover() {
        let r = req("A.01", Lot::Mvp, Status::Implemented, &[Level::Unit]);
        let (_, errs) = evaluate(
            &[r],
            &[link("A.01", "u", Level::Unit)],
            &results(&[("u", Outcome::Failed)]),
            Lot::Mvp,
            false,
        );
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn not_run_test_does_not_cover() {
        let r = req("A.01", Lot::Mvp, Status::Implemented, &[Level::Unit]);
        let (_, errs) = evaluate(
            &[r],
            &[link("A.01", "u", Level::Unit)],
            &Results::new(),
            Lot::Mvp,
            false,
        );
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn draft_requirements_do_not_fail_outside_release() {
        let r = req("A.01", Lot::Mvp, Status::Draft, &[Level::Unit]);
        let (_, errs) = evaluate(&[r], &[], &Results::new(), Lot::Mvp, false);
        assert_eq!(errs, Vec::<Diagnostic>::new());
    }

    #[test]
    fn release_requires_all_current_lot_implemented() {
        let reqs = [
            req("A.01", Lot::Mvp, Status::Validated, &[Level::Unit]),
            req("B.01", Lot::Mvp, Status::Implemented, &[Level::Unit]),
        ];
        let l = [
            link("A.01", "a", Level::Unit),
            link("B.01", "b", Level::Unit),
        ];
        let outcomes = results(&[("a", Outcome::Passed), ("b", Outcome::Passed)]);
        let (_, errs) = evaluate(&reqs, &l, &outcomes, Lot::Mvp, true);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("A.01 : non implémentée"));
    }

    #[test]
    fn other_lots_are_ignored_by_gate() {
        let r = req("A.01", Lot::V2, Status::Implemented, &[Level::Unit]);
        let (_, errs) = evaluate(&[r], &[], &Results::new(), Lot::Mvp, true);
        assert_eq!(errs, Vec::<Diagnostic>::new());
    }

    #[test]
    fn markdown_matrix_lists_each_requirement_with_status() {
        let reqs = [
            req(
                "A.01",
                Lot::Mvp,
                Status::Implemented,
                &[Level::Unit, Level::E2e],
            ),
            req("B.01", Lot::Mvp, Status::Draft, &[Level::Ui]),
        ];
        let l = [
            link("A.01", "a_unit", Level::Unit),
            link("A.01", "a_e2e", Level::E2e),
        ];
        let (cov, _) = evaluate(
            &reqs,
            &l,
            &results(&[("a_unit", Outcome::Passed)]),
            Lot::Mvp,
            false,
        );
        let md = render_markdown(&cov);
        assert!(md.contains("| A.01 | Titre A.01 | F6 | MVP | implemented | unit, e2e | ✓ a_unit (unit)<br>∅ a_e2e (e2e) | ✗ non couverte |"), "{md}");
        assert!(
            md.contains("| B.01 | Titre B.01 | F6 | MVP | draft | ui |  | à faire |"),
            "{md}"
        );
        assert!(render_html(&cov).contains("<td>A.01</td>"));
    }

    #[test]
    fn result_of_another_level_does_not_cover() {
        let r = req("A.01", Lot::Mvp, Status::Implemented, &[Level::E2e]);
        let l = [link("A.01", "ouvre le fichier", Level::E2e)];
        let res: HashMap<(Level, String), Outcome> =
            [((Level::Ui, "ouvre le fichier".to_owned()), Outcome::Passed)]
                .into_iter()
                .collect();
        let (_, errs) = evaluate(&[r], &l, &res, Lot::Mvp, false);
        assert_eq!(
            errs.len(),
            1,
            "un résultat ui ne doit pas couvrir un lien e2e"
        );
    }

    #[test]
    fn duplicate_ts_titles_fail_gate() {
        let r = req("A.01", Lot::Mvp, Status::Draft, &[Level::Ui]);
        let mut a = link("A.01", "affiche l'image", Level::Ui);
        a.file = PathBuf::from("ui/src/A.test.tsx");
        let mut b = link("A.01", "affiche l'image", Level::Ui);
        b.file = PathBuf::from("ui/src/B.test.tsx");
        let (_, errs) = evaluate(&[r], &[a, b], &Results::new(), Lot::Mvp, false);
        assert!(
            errs.iter()
                .any(|e| e.message.contains("titre de test en double")),
            "{errs:?}"
        );
    }
}

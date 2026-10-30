//! Résultats d'exécution des tests : `JUnit` (`nextest`, `WebdriverIO`), JSON `Playwright`, JSON `Vitest`.

use quick_xml::Reader;
use quick_xml::events::Event;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Outcome {
    Passed,
    Skipped,
    NotRun,
    Failed,
}

/// Fusion de deux résultats pour le même nom de test : l'échec l'emporte, puis le non-passage.
fn merge(map: &mut HashMap<String, Outcome>, name: String, o: Outcome) {
    map.entry(name)
        .and_modify(|cur| {
            if o == Outcome::Failed || (*cur == Outcome::Passed && o != Outcome::Passed) {
                *cur = o;
            }
        })
        .or_insert(o);
}

fn attr(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    e.attributes()
        .filter_map(Result::ok)
        .find(|a| a.key.as_ref() == key)
        .and_then(|a| a.unescape_value().ok().map(std::borrow::Cow::into_owned))
}

/// `JUnit` : `classname::name` (tests Rust de `nextest`) et `name` seul (titres `WebdriverIO`).
fn parse_junit(text: &str, out: &mut HashMap<String, Outcome>) -> anyhow::Result<()> {
    let mut reader = Reader::from_str(text);
    let mut current: Option<(String, Outcome)> = None;
    let finish = |c: Option<(String, Outcome)>, out: &mut HashMap<String, Outcome>| {
        if let Some((key, o)) = c {
            if let Some((_, name)) = key.split_once('\u{0}') {
                merge(out, name.to_owned(), o);
            }
            merge(out, key.replace('\u{0}', "::"), o);
        }
    };
    loop {
        match reader.read_event()? {
            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == b"testcase" => {
                finish(current.take(), out);
                let name = attr(&e, b"name").unwrap_or_default();
                let class = attr(&e, b"classname").unwrap_or_default();
                current = Some((format!("{class}\u{0}{name}"), Outcome::Passed));
            }
            Event::Start(e) | Event::Empty(e) => {
                if let Some(c) = current.as_mut() {
                    match e.name().as_ref() {
                        b"failure" | b"error" => c.1 = Outcome::Failed,
                        b"skipped" => c.1 = Outcome::Skipped,
                        _ => {}
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    finish(current, out);
    Ok(())
}

fn playwright_specs(suite: &Value, out: &mut HashMap<String, Outcome>) {
    for spec in suite["specs"].as_array().into_iter().flatten() {
        let title = spec["title"].as_str().unwrap_or_default().to_owned();
        let statuses: Vec<&str> = spec["tests"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|t| t["results"].as_array().into_iter().flatten())
            .filter_map(|r| r["status"].as_str())
            .collect();
        let o = if statuses.is_empty() {
            Outcome::NotRun
        } else if spec["ok"].as_bool() == Some(true) && statuses.contains(&"passed") {
            Outcome::Passed
        } else if statuses.iter().all(|s| *s == "skipped") {
            Outcome::Skipped
        } else {
            Outcome::Failed
        };
        merge(out, title, o);
    }
    for sub in suite["suites"].as_array().into_iter().flatten() {
        playwright_specs(sub, out);
    }
}

fn parse_json(v: &Value, out: &mut HashMap<String, Outcome>) {
    if let Some(suites) = v["suites"].as_array() {
        for s in suites {
            playwright_specs(s, out);
        }
    }
    for file in v["testResults"].as_array().into_iter().flatten() {
        for a in file["assertionResults"].as_array().into_iter().flatten() {
            let o = match a["status"].as_str() {
                Some("passed") => Outcome::Passed,
                Some("failed") => Outcome::Failed,
                Some("skipped" | "pending" | "todo" | "disabled") => Outcome::Skipped,
                _ => Outcome::NotRun,
            };
            merge(out, a["title"].as_str().unwrap_or_default().to_owned(), o);
        }
    }
}

/// Charge les fichiers de résultats (format reconnu à l'extension : `.xml` `JUnit`, `.json`).
pub fn load_results(paths: &[PathBuf]) -> anyhow::Result<HashMap<String, Outcome>> {
    let mut out = HashMap::new();
    for p in paths {
        let text =
            std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("{} : {e}", p.display()))?;
        if p.extension().is_some_and(|e| e == "xml") {
            parse_junit(&text, &mut out)
                .map_err(|e| anyhow::anyhow!("{} : JUnit invalide : {e}", p.display()))?;
        } else {
            let v: Value = serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("{} : JSON invalide : {e}", p.display()))?;
            parse_json(&v, &mut out);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/results")
            .join(name)
    }

    #[test]
    fn parses_nextest_junit() {
        let r = load_results(&[fixture("junit.xml")]).unwrap();
        assert_eq!(r["demo::tests::adds"], Outcome::Passed);
        assert_eq!(r["demo::tests::broken"], Outcome::Failed);
        assert_eq!(r["demo::it::integration"], Outcome::Skipped);
    }

    #[test]
    fn parses_playwright_json() {
        let r = load_results(&[fixture("playwright.json")]).unwrap();
        assert_eq!(r["navigue à l'image suivante"], Outcome::Passed);
        assert_eq!(r["aller à"], Outcome::Failed);
    }

    #[test]
    fn parses_vitest_json() {
        let r = load_results(&[fixture("vitest.json")]).unwrap();
        assert_eq!(r["affiche une coche pour ok"], Outcome::Passed);
        assert_eq!(r["clignote"], Outcome::Skipped);
    }

    #[test]
    fn missing_results_file_is_an_error() {
        assert!(load_results(&[fixture("absent.xml")]).is_err());
    }
}

//! Outillage du dépôt : traçabilité exigences ↔ tests, pipeline de vérification locale.

mod ci;
mod matrix;
mod requirements;
mod results;
mod scan_rust;
mod scan_ts;

use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(about = "Outillage du dépôt Media Analyzer")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Génère la matrice de traçabilité et applique la garde.
    Trace(TraceArgs),
    /// Enchaîne toutes les vérifications de la CI.
    Ci {
        /// Applique aussi la garde de livraison de la traçabilité.
        #[arg(long)]
        release: bool,
    },
}

#[derive(Args)]
struct TraceArgs {
    /// Fichiers de résultats, préfixés de leur niveau : `unit=…`, `ui=…`, `e2e=…`
    /// (`JUnit` `.xml`, `Playwright` / `Vitest` `.json`). Sans préfixe : `.xml` = unit, `.json` = ui.
    #[arg(long = "results", value_parser = parse_results_arg)]
    results: Vec<(requirements::Level, PathBuf)>,
    /// Garde de livraison : toutes les exigences du lot courant doivent être implémentées et couvertes.
    #[arg(long)]
    release: bool,
    /// Dossier de sortie de la matrice.
    #[arg(long, default_value = "target/trace")]
    out: PathBuf,
}

fn parse_results_arg(v: &str) -> Result<(requirements::Level, PathBuf), String> {
    use requirements::Level;
    match v.split_once('=') {
        Some(("unit", p)) => Ok((Level::Unit, p.into())),
        Some(("ui", p)) => Ok((Level::Ui, p.into())),
        Some(("e2e", p)) => Ok((Level::E2e, p.into())),
        Some((other, _)) => Err(format!("niveau inconnu « {other} » (unit, ui, e2e)")),
        None if Path::new(v)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("xml")) =>
        {
            Ok((Level::Unit, v.into()))
        }
        None => Ok((Level::Ui, v.into())),
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask est dans le dépôt")
        .to_path_buf()
}

/// Exécute la commande `trace` ; renvoie `false` si la garde échoue.
fn trace(args: &TraceArgs) -> anyhow::Result<bool> {
    let root = repo_root();
    let (config, reqs) = match requirements::load(&root.join("requirements")) {
        Ok(v) => v,
        Err(diags) => {
            for d in diags {
                eprintln!("erreur : {d}");
            }
            return Ok(false);
        }
    };
    let (mut links, mut warnings) = scan_rust::scan_rust(&root);
    let (ts_links, ts_warnings) = scan_ts::scan_ts(&root);
    links.extend(ts_links);
    warnings.extend(ts_warnings);
    for w in &warnings {
        eprintln!("{w}");
    }
    let results = results::load_results(&args.results)?;
    let (cov, errors) = matrix::evaluate(&reqs, &links, &results, config.current_lot, args.release);

    let out = if args.out.is_absolute() {
        args.out.clone()
    } else {
        root.join(&args.out)
    };
    std::fs::create_dir_all(&out)?;
    std::fs::write(out.join("matrix.md"), matrix::render_markdown(&cov))?;
    std::fs::write(out.join("matrix.html"), matrix::render_html(&cov))?;
    println!(
        "{} exigences, {} liens de test, matrice : {}",
        reqs.len(),
        links.len(),
        out.join("matrix.md").display()
    );
    for e in &errors {
        eprintln!("erreur : {e}");
    }
    Ok(errors.is_empty())
}

fn main() -> anyhow::Result<ExitCode> {
    let ok = match Cli::parse().cmd {
        Cmd::Trace(args) => trace(&args)?,
        Cmd::Ci { release } => ci::run(&repo_root(), release)?,
    };
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

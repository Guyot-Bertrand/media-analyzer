//! Outillage du dépôt : traçabilité exigences ↔ tests, pipeline de vérification locale.

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
    Ci,
}

#[derive(Args)]
struct TraceArgs {
    /// Fichiers de résultats (`JUnit` `.xml`, `Playwright` / `Vitest` `.json`).
    #[arg(long = "results")]
    results: Vec<PathBuf>,
    /// Garde de livraison : toutes les exigences du lot courant doivent être implémentées et couvertes.
    #[arg(long)]
    release: bool,
    /// Dossier de sortie de la matrice.
    #[arg(long, default_value = "target/trace")]
    out: PathBuf,
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
        Cmd::Ci => todo!("Tâche 7"),
    };
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

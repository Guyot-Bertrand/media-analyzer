//! Outillage du dépôt : traçabilité exigences ↔ tests, pipeline de vérification locale.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "Outillage du dépôt Media Analyzer")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Génère la matrice de traçabilité et applique la garde.
    Trace,
    /// Enchaîne toutes les vérifications de la CI.
    Ci,
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Trace => todo!("Tâche 5"),
        Cmd::Ci => todo!("Tâche 7"),
    }
}

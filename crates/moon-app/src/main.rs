//! Entry point of the `moon` binary.

use clap::Parser;

#[derive(Parser)]
#[command(name = "moon-trade", version, about = "Gestor de cartera para IOL")]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}

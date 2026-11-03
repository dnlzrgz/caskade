use std::io::{self, Write};

use clap::Parser;

use caskade::{
    cli::{Cli, Commands},
    error::Result,
    kv::{Durability, KvStore, LOG_FILE},
};

fn main() {
    if let Err(e) = run() {
        eprintln!("caskade: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let mut store = KvStore::open(&cli.db)?;
    if matches!(
        &cli.command,
        Commands::Set { sync: true, .. } | Commands::Rm { sync: true, .. }
    ) {
        store.set_durability(Durability::Always);
    }

    let dropped = store.dropped_bytes();
    if dropped > 0 {
        let unit = if dropped == 1 { "byte" } else { "bytes" };
        eprintln!("caskade: dropped {dropped} {unit} of torn tail from {LOG_FILE}");
    }

    match cli.command {
        Commands::Get { key } => {
            let mut out = io::stdout().lock();
            out.write_all(&store.get(key.as_bytes())?)?;
            out.write_all(b"\n")?;
        }
        Commands::Set { key, value, .. } => store.set(key, value)?,
        Commands::Rm { key, .. } => store.remove(key.as_bytes())?,
    }

    Ok(())
}

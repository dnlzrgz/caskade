use clap::Parser;

use caskade::{
    cli::{Cli, Commands},
    error::Result,
    kv::{KvStore, LOG_FILE},
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
    let dropped = store.dropped_bytes();
    if dropped > 0 {
        let unit = if dropped == 1 { "byte" } else { "bytes" };
        eprintln!("caskade: dropped {dropped} {unit} of torn tail from {LOG_FILE}");
    }

    match cli.command {
        Commands::Get { key } => println!("{}", store.get(&key)?),
        Commands::Set { key, value, sync } => {
            store.set(key, value)?;
            if sync {
                store.sync()?;
            }
        }
        Commands::Rm { key, sync } => {
            store.remove(&key)?;
            if sync {
                store.sync()?;
            }
        }
    }

    Ok(())
}

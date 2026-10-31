use clap::Parser;

use caskade::{
    cli::{Cli, Commands},
    error::{KvError, Result},
    kv::KvStore,
};

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut store = KvStore::open(std::env::current_dir()?)?;

    match cli.command {
        Commands::Get { key } => match store.get(key) {
            Ok(value) => println!("{value}"),
            Err(KvError::KeyNotFound) => println!("key not found"),
            Err(e) => return Err(e),
        },
        Commands::Set { key, value } => store.set(key, value)?,
        Commands::Rm { key } => store.remove(key)?,
    }

    Ok(())
}

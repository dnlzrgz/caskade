use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about, name = "caskade")]
pub struct Cli {
    /// Store directory.
    #[arg(long = "db", global = true, value_name = "DIR", default_value = ".")]
    pub db: PathBuf,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    Get {
        key: String,
    },
    Set {
        key: String,
        value: String,
        /// Sync to disk before exiting.
        #[arg(long)]
        sync: bool,
    },
    Rm {
        key: String,
        /// Sync to disk before exiting.
        #[arg(long)]
        sync: bool,
    },
}

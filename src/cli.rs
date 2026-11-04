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
    /// Print the value of a key.
    Get { key: String },
    /// Set a key to a value.
    Set {
        key: String,
        value: String,
        /// Sync to disk before exiting.
        #[arg(long)]
        sync: bool,
    },
    /// Remove a key.
    Rm {
        key: String,
        /// Sync to disk before exiting.
        #[arg(long)]
        sync: bool,
    },
    /// Rewrite the log with live records only.
    Compact,
    /// Print the size and shape of the log.
    Stats,
}

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about, name = "caskade")]
pub struct Cli {
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
        /// Sync this write to disk before exiting.
        #[arg(long)]
        sync: bool,
    },
    Rm {
        key: String,
        /// Sync this write to disk before exiting.
        #[arg(long)]
        sync: bool,
    },
}

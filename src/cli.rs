use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about, name = "caskade")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    Get { key: String },
    Set { key: String, value: String },
    Rm { key: String },
}

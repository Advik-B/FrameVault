//! FrameVault CLI: `framevault encode <in> <out.mp4>` / `framevault decode <video> [dir]`.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "framevault",
    about = "Encode arbitrary files into YouTube-survivable video and decode them back",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Encode a file into a FrameVault MP4 (video block channel).
    Encode {
        /// Input file to encode.
        input: PathBuf,
        /// Output MP4 path.
        output: PathBuf,
        /// Memory budget for RS encoding (e.g. "512M", "2G"). Defaults to 25% of total
        /// system RAM.
        #[arg(long)]
        memory: Option<String>,
    },
    /// Decode a FrameVault MP4 back into the original file.
    Decode {
        /// Input MP4 (downloaded at 1080p).
        video: PathBuf,
        /// Directory to write the recovered file into.
        #[arg(default_value = ".")]
        output_dir: PathBuf,
        /// Memory budget for RS decoding (e.g. "512M", "2G"). Defaults to 25% of total
        /// system RAM.
        #[arg(long)]
        memory: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Encode { input, output, memory } => {
            framevault::encode::encode(&input, &output, memory.as_deref())?;
        }
        Command::Decode { video, output_dir, memory } => {
            std::fs::create_dir_all(&output_dir)?;
            let report = framevault::decode::decode(&video, &output_dir, memory.as_deref())?;
            println!(
                "\nDone. '{}' recovered successfully ({:?} strategy).",
                report.filename, report.strategy
            );
        }
    }
    Ok(())
}

use std::{fs, path::PathBuf, time::Instant};

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(about, version, author)]
struct Args {
    #[arg(short, long)]
    file: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let bytes =
        fs::read(&args.file).with_context(|| format!("failed to read {}", args.file.display()))?;
    let start = Instant::now();
    let decompilation = lua51_lifter::decompile(&bytes)?;
    let output = args.file.with_extension("dec.51.lua");
    fs::write(
        &output,
        format!(
            "-- Decompile time: {:?}\n{}",
            start.elapsed(),
            decompilation.source
        ),
    )
    .with_context(|| format!("failed to write {}", output.display()))?;
    Ok(())
}

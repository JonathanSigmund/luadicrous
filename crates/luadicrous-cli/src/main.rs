use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use luadicrous_core::{Analysis, CustomOpcodeTable, Engine, Inventory};
use serde::{Deserialize, Serialize};

#[derive(Debug, Parser)]
#[command(name = "luadicrous-rs", version, about)]
struct Arguments {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Recover correctness-safe Lua or Luau source.
    Decompile {
        input: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[arg(long)]
        opcode_table: Option<PathBuf>,
    },
    /// Decode every instruction without attempting source reconstruction.
    Disassemble {
        input: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
        #[arg(long)]
        opcode_table: Option<PathBuf>,
    },
    /// Strictly parse a Lua or Luau chunk and print its structural inventory.
    Inspect {
        input: PathBuf,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        opcode_table: Option<PathBuf>,
    },
    /// Deserialize a Lua or Luau chunk and print its structural inventory.
    Deserialize {
        input: PathBuf,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        opcode_table: Option<PathBuf>,
    },
    /// Compare the Rust structural inventory with Python Luadicrous.
    Parity {
        input: PathBuf,
        #[arg(long, default_value = "../luadicrous")]
        python_root: PathBuf,
        #[arg(long, default_value = "pypy3")]
        python_runtime: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct OracleInventory {
    luau_version: u8,
    types_version: u8,
    string_count: usize,
    proto_count: usize,
    main_proto: u32,
    consumed_bytes: usize,
    trailing_bytes: usize,
    instruction_counts: Vec<usize>,
    constant_counts: Vec<usize>,
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    match arguments.command {
        Commands::Decompile {
            input,
            output,
            opcode_table,
        } => {
            let bytes = read_file(&input)?;
            let decompilation = engine(opcode_table.as_deref())?
                .decompile(&bytes)
                .with_context(|| format!("failed to decompile {}", input.display()))?;
            write_output(output.as_deref(), &decompilation.source)?;
        }
        Commands::Disassemble {
            input,
            output,
            opcode_table,
        } => {
            let bytes = read_file(&input)?;
            let assembly = engine(opcode_table.as_deref())?
                .disassemble(&bytes)
                .with_context(|| format!("failed to disassemble {}", input.display()))?;
            write_output(output.as_deref(), &assembly)?;
        }
        Commands::Inspect {
            input,
            json,
            opcode_table,
        } => {
            let analysis = analyze_file(&input, opcode_table.as_deref())?;
            print_analysis(&input, &analysis, json)?;
        }
        Commands::Deserialize {
            input,
            json,
            opcode_table,
        } => {
            let analysis = analyze_file(&input, opcode_table.as_deref())?;
            print_analysis(&input, &analysis, json)?;
        }
        Commands::Parity {
            input,
            python_root,
            python_runtime,
            json,
        } => run_parity(&input, &python_root, &python_runtime, json)?,
    }
    Ok(())
}

fn analyze_file(path: &Path, opcode_table: Option<&Path>) -> Result<Analysis> {
    let bytes = read_file(path)?;
    engine(opcode_table)?
        .analyze(&bytes)
        .with_context(|| format!("failed to analyze {}", path.display()))
}

fn engine(opcode_table: Option<&Path>) -> Result<Engine> {
    let Some(path) = opcode_table else {
        return Ok(Engine::default());
    };
    let bytes = read_file(path)?;
    let table = CustomOpcodeTable::from_json_slice(&bytes)
        .with_context(|| format!("failed to parse opcode table {}", path.display()))?;
    Ok(Engine::default().with_opcode_table(table))
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).with_context(|| format!("failed to read {}", path.display()))
}

fn write_output(path: Option<&Path>, contents: &str) -> Result<()> {
    if let Some(path) = path {
        fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))?;
    } else {
        print!("{contents}");
    }
    Ok(())
}

fn print_analysis(path: &Path, analysis: &Analysis, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(analysis)?);
        return Ok(());
    }

    let inventory = &analysis.inventory;
    println!("file       : {}", path.display());
    if inventory.wire_flavor == "lua51" {
        println!("format     : PUC Lua 5.1");
    } else {
        println!(
            "format     : Luau v{} / types v{} / {}",
            inventory.luau_version, inventory.types_version, inventory.wire_flavor
        );
    }
    println!("strings    : {}", inventory.string_count);
    println!("prototypes : {}", inventory.proto_count);
    println!("main       : {}", inventory.main_proto);
    println!("consumed   : {} bytes", inventory.consumed_bytes);
    println!("trailing   : {} bytes", inventory.trailing_bytes);
    if inventory.opcode_encodings.iter().all(|encoding| {
        inventory
            .opcode_encodings
            .first()
            .is_some_and(|first| first == encoding)
    }) {
        println!(
            "opcodes    : {}",
            inventory
                .opcode_encodings
                .first()
                .map_or("n/a", String::as_str)
        );
    } else {
        println!("opcodes    : mixed");
    }
    println!(
        "quality    : {} ({} errors, {} warnings)",
        analysis.quality.score, analysis.quality.errors, analysis.quality.warnings
    );
    for diagnostic in &analysis.module.diagnostics {
        println!(
            "  {:?} {:?}: {}",
            diagnostic.severity, diagnostic.code, diagnostic.message
        );
    }
    Ok(())
}

fn run_parity(input: &Path, python_root: &Path, python_runtime: &str, json: bool) -> Result<()> {
    let analysis = analyze_file(input, None)?;
    let oracle = run_python_oracle(input, python_root, python_runtime)?;
    let rust = comparable_inventory(&analysis.inventory);
    let matched = rust == oracle;

    if json {
        let value = serde_json::json!({
            "matched": matched,
            "rust": rust,
            "python": oracle,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else if matched {
        println!("parity: exact structural match for {}", input.display());
    } else {
        println!("parity: mismatch for {}", input.display());
        println!("rust   : {rust:#?}");
        println!("python : {oracle:#?}");
    }

    if !matched {
        bail!("Rust parser inventory does not match the Python oracle");
    }
    Ok(())
}

fn comparable_inventory(inventory: &Inventory) -> OracleInventory {
    OracleInventory {
        luau_version: inventory.luau_version,
        types_version: inventory.types_version,
        string_count: inventory.string_count,
        proto_count: inventory.proto_count,
        main_proto: inventory.main_proto,
        consumed_bytes: inventory.consumed_bytes,
        trailing_bytes: inventory.trailing_bytes,
        instruction_counts: inventory.instruction_counts.clone(),
        constant_counts: inventory.constant_counts.clone(),
    }
}

fn run_python_oracle(
    input: &Path,
    python_root: &Path,
    python_runtime: &str,
) -> Result<OracleInventory> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tools/python_oracle.py");
    let output = Command::new(python_runtime)
        .arg(&script)
        .arg("--python-root")
        .arg(python_root)
        .arg(input)
        .output()
        .with_context(|| format!("failed to start {python_runtime}"))?;
    if !output.status.success() {
        bail!(
            "Python oracle failed with {}:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "Python oracle returned invalid JSON:\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

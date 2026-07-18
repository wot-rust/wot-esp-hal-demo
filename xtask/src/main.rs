use std::{process::Command, str};

use clap::{Parser, Subcommand};

/// Demo definitions: (binary name, package, target triple)
const DEMOS: &[(&str, &str, &str)] = &[
    ("thermometer", "demo-c3", "riscv32imc-unknown-none-elf"),
    ("light", "demo-c3", "riscv32imc-unknown-none-elf"),
    ("button", "demo-c3", "riscv32imc-unknown-none-elf"),
    ("fan", "demo-c6", "riscv32imac-unknown-none-elf"),
];

fn demo_names() -> Vec<&'static str> {
    DEMOS.iter().map(|(name, _, _)| *name).collect()
}

fn find_demo(name: &str) -> (&'static str, &'static str, &'static str) {
    *DEMOS
        .iter()
        .find(|(n, _, _)| *n == name)
        .unwrap_or_else(|| panic!("unknown demo '{name}', available: {:?}", demo_names()))
}

#[derive(Parser)]
#[command(name = "xtask")]
#[command(about = "Build and run wot-esp-hal-demo demos", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build a demo binary
    Build {
        /// Demo name: thermometer, light, button, fan
        demo: String,
    },
    /// Build and flash a demo to the connected board
    Run {
        /// Demo name: thermometer, light, button, fan
        demo: String,
        /// Serial port (e.g. /dev/cu.usbmodem101). If omitted, espflash auto-detects.
        #[arg(long)]
        port: Option<String>,
    },
    /// `cargo check` every demo for its target triple
    CheckAll,
    /// List available demos
    List,
}

fn cargo(demo: &str, action: &str, extra_args: &[&str]) {
    let (bin, pkg, target) = find_demo(demo);
    let mut args = vec![action, "-p", pkg, "--bin", bin, "--target", target];
    args.push("-Z");
    args.push("build-std=alloc,core");
    args.extend_from_slice(extra_args);

    println!("$ cargo {}", args.join(" "));
    let status = Command::new("cargo").args(&args).status();
    match status {
        Ok(s) if !s.success() => std::process::exit(1),
        Ok(_) => {}
        Err(e) => {
            eprintln!("cargo error: {e}");
            std::process::exit(1);
        }
    }
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Build { demo } => {
            cargo(&demo, "build", &[]);
        }
        Commands::Run { demo, port } => {
            let (bin, _, target) = find_demo(&demo);
            // Build first
            cargo(&demo, "build", &[]);

            // Then flash with espflash
            let binary = format!("target/{target}/debug/{bin}");
            let mut esp_args = vec!["flash"];
            if let Some(p) = &port {
                esp_args.push("--port");
                esp_args.push(p);
            }
            esp_args.push("--monitor");
            esp_args.push(&binary);

            println!("$ espflash {}", esp_args.join(" "));
            let status = Command::new("espflash").args(&esp_args).status();
            match status {
                Ok(s) if !s.success() => std::process::exit(1),
                Ok(_) => {}
                Err(e) => {
                    eprintln!("espflash error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Commands::CheckAll => {
            let mut failed = false;
            for (name, _, _) in DEMOS {
                println!("=== check {name} ===");
                let (bin, pkg, target) = find_demo(name);
                let args = [
                    "check",
                    "-p",
                    pkg,
                    "--bin",
                    bin,
                    "--target",
                    target,
                    "-Z",
                    "build-std=alloc,core",
                ];
                println!("$ cargo {}", args.join(" "));
                match Command::new("cargo").args(args).status() {
                    Ok(s) if s.success() => {}
                    Ok(_) => {
                        eprintln!("check failed for {name}");
                        failed = true;
                    }
                    Err(e) => {
                        eprintln!("cargo error for {name}: {e}");
                        failed = true;
                    }
                }
            }
            if failed {
                std::process::exit(1);
            }
            println!("All demos checked successfully.");
        }
        Commands::List => {
            println!("Available demos:");
            for (name, pkg, target) in DEMOS {
                println!("  {name:<14} ({pkg}, {target})");
            }
        }
    }
}

use std::fs;
use std::path::Path;
use std::process;

use clap::{Parser, Subcommand};
use yansi::Paint;

use bit::engine;
use bit::loader;
use bit::output::Output;
use bit::provider::ProviderRegistry;
use bit::providers::docker::DockerProvider;
use bit::providers::exec::ExecProvider;
use bit::state;
use bit::value::Map;

#[derive(Parser)]
#[command(name = "bit", about = "bit — Build It", version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show what would change without applying
    Plan {
        /// Target to plan (default: all blocks)
        target: Option<String>,
    },
    /// Apply all blocks (or a target)
    #[command(alias = "build")]
    Apply {
        /// Target to apply (default: all blocks)
        target: Option<String>,
    },
    /// Destroy blocks in reverse dependency order
    #[command(alias = "clean")]
    Destroy {
        /// Target to destroy (default: all blocks)
        target: Option<String>,
    },
    /// Run test blocks and their dependencies
    Test,
    /// List top-level targets
    List,
    /// Dump evaluated inputs and stored outputs
    Dump {
        /// Target to dump (default: all blocks)
        target: Option<String>,
    },
}

fn default_registry() -> ProviderRegistry {
    let mut reg = ProviderRegistry::new();
    reg.register(Box::new(ExecProvider));
    reg.register(Box::new(DockerProvider));
    reg
}

fn load_module(registry: &ProviderRegistry) -> (bit::dag::Dag, loader::BaseScope, Box<dyn bit::state::StateStore>) {
    let root = Path::new(".");
    let store = Box::new(state::default_store(root));
    let source = match fs::read_to_string("BUILD.bit") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{} cannot read BUILD.bit: {e}", "error:".red().bold());
            process::exit(1);
        }
    };
    let module = match bit::parser::parse(&source, "BUILD.bit") {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{} {e}", "error:".red().bold());
            process::exit(1);
        }
    };
    let (dag, base) = match loader::load(&module, &Map::new(), registry, store.as_ref()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{} {e}", "error:".red().bold());
            process::exit(1);
        }
    };
    (dag, base, store)
}

fn main() {
    let cli = Cli::parse();
    let registry = default_registry();

    match cli.command {
        Command::Plan { target } => {
            let (mut dag, base, store) = load_module(&registry);
            let names = dag.block_names();
            let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
            let output = Output::new(&name_refs);
            match engine::plan(&mut dag, &base, store.as_ref(), &output, target.as_deref()) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("{} {e}", "error:".red().bold());
                    process::exit(1);
                }
            }
        }
        Command::Apply { target } => {
            let (mut dag, base, store) = load_module(&registry);
            let names = dag.block_names();
            let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
            let output = Output::new(&name_refs);
            match engine::apply(&mut dag, &base, store.as_ref(), &output, target.as_deref()) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("{} {e}", "error:".red().bold());
                    process::exit(1);
                }
            }
        }
        Command::Test => {
            let (mut dag, base, store) = load_module(&registry);
            let names = dag.block_names();
            let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
            let output = Output::new(&name_refs);
            match engine::test(&mut dag, &base, store.as_ref(), &output) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("{} {e}", "error:".red().bold());
                    process::exit(1);
                }
            }
        }
        Command::Destroy { target } => {
            let (mut dag, _base, store) = load_module(&registry);
            let names = dag.block_names();
            let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
            let output = Output::new(&name_refs);
            match engine::destroy(&mut dag, store.as_ref(), &output, target.as_deref()) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("{} {e}", "error:".red().bold());
                    process::exit(1);
                }
            }
        }
        Command::Dump { target } => {
            let (mut dag, base, _store) = load_module(&registry);
            match engine::dump(&mut dag, &base, target.as_deref()) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("{} {e}", "error:".red().bold());
                    process::exit(1);
                }
            }
        }
        Command::List => {
            let (dag, _base, _store) = load_module(&registry);
            let targets = dag.targets();
            if targets.is_empty() {
                println!("No targets defined.");
            } else {
                let mut names: Vec<_> = targets.keys().collect();
                names.sort();
                for name in names {
                    let target = &targets[name];
                    match &target.doc {
                        Some(doc) => println!("  {} — {}", name.bold(), doc.dim()),
                        None => println!("  {}", name.bold()),
                    }
                }
            }
        }
    }
}

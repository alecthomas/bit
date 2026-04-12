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
use bit::providers::go::GoProvider;
use bit::state;
use bit::value::Map;

#[derive(Parser)]
#[command(name = "bit", about = "bit — Build It", version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    /// Number of parallel jobs (default: number of CPUs)
    #[arg(short = 'j', long = "jobs", global = true)]
    jobs: Option<usize>,

    /// Set a parameter value (e.g. -p verbose=true)
    #[arg(short = 'p', long = "param", global = true, value_name = "KEY=VALUE")]
    params: Vec<String>,

    #[command(subcommand)]
    command: Option<Command>,
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
    reg.register(Box::new(GoProvider));
    reg
}

/// Parse "key=value" strings into a typed Value Map.
///
/// Values are inferred as bool, int, or string.
fn parse_params(raw: &[String]) -> Map {
    let mut params = Map::new();
    for item in raw {
        let Some((key, val)) = item.split_once('=') else {
            eprintln!("{} invalid param (expected key=value): {item}", "error:".red().bold());
            process::exit(1);
        };
        let value = if val == "true" {
            bit::value::Value::Bool(true)
        } else if val == "false" {
            bit::value::Value::Bool(false)
        } else if let Ok(n) = val.parse::<i64>() {
            bit::value::Value::Int(n)
        } else {
            bit::value::Value::Str(val.to_owned())
        };
        params.insert(key.to_owned(), value);
    }
    params
}

fn load_module(
    registry: &ProviderRegistry,
    params: &Map,
) -> (bit::dag::Dag, loader::BaseScope, Box<dyn bit::state::StateStore>) {
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
    let (dag, base) = match loader::load(&module, params, registry, store.as_ref()) {
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
    let params = parse_params(&cli.params);
    let jobs = cli
        .jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));

    let command = cli.command.unwrap_or(Command::Apply { target: None });
    match command {
        Command::Plan { target } => {
            let (mut dag, base, store) = load_module(&registry, &params);
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
            let (mut dag, base, store) = load_module(&registry, &params);
            let names = dag.block_names();
            let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
            let output = Output::new(&name_refs);
            match engine::apply(&mut dag, &base, store.as_ref(), &output, target.as_deref(), jobs) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("{} {e}", "error:".red().bold());
                    process::exit(1);
                }
            }
        }
        Command::Test => {
            let (mut dag, base, store) = load_module(&registry, &params);
            let names = dag.block_names();
            let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
            let output = Output::new(&name_refs);
            match engine::test(&mut dag, &base, store.as_ref(), &output, jobs) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("{} {e}", "error:".red().bold());
                    process::exit(1);
                }
            }
        }
        Command::Destroy { target } => {
            let (mut dag, _base, store) = load_module(&registry, &params);
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
            let (mut dag, base, _store) = load_module(&registry, &params);
            match engine::dump(&mut dag, &base, target.as_deref()) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("{} {e}", "error:".red().bold());
                    process::exit(1);
                }
            }
        }
        Command::List => {
            let (dag, _base, _store) = load_module(&registry, &params);
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

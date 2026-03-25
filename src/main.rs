use std::fs;
use std::path::Path;
use std::process;

use clap::{Parser, Subcommand};

use bit::engine;
use bit::loader;
use bit::provider::{PlanAction, ProviderRegistry};
use bit::providers::exec::ExecProvider;
use bit::state;
use bit::value::Map;

#[derive(Parser)]
#[command(name = "bit", about = "bit — Build It")]
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
    Apply {
        /// Target to apply (default: all blocks)
        target: Option<String>,
    },
    /// Destroy blocks in reverse dependency order
    Destroy {
        /// Target to destroy (default: all blocks)
        target: Option<String>,
    },
    /// List top-level targets
    List,
}

fn default_registry() -> ProviderRegistry {
    let mut reg = ProviderRegistry::new();
    reg.register(Box::new(ExecProvider));
    reg
}

fn load_module(registry: &ProviderRegistry) -> (bit::dag::Dag, loader::BaseScope, Box<dyn bit::state::StateStore>) {
    let root = Path::new(".");
    let store = Box::new(state::default_store(root));
    let source = match fs::read_to_string("BUILD.bit") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read BUILD.bit: {e}");
            process::exit(1);
        }
    };
    let module = match bit::parser::parse(&source) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            process::exit(1);
        }
    };
    let (dag, base) = match loader::load(&module, &Map::new(), registry, store.as_ref()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
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
            let (mut dag, base, _store) = load_module(&registry);
            match engine::plan(&mut dag, &base, target.as_deref()) {
                Ok(plans) => {
                    if plans.is_empty() {
                        println!("No blocks to plan.");
                        return;
                    }
                    for bp in &plans {
                        let symbol = match bp.plan.action {
                            PlanAction::Create => "+",
                            PlanAction::Update => "~",
                            PlanAction::Replace => "!",
                            PlanAction::Destroy => "-",
                            PlanAction::None => " ",
                        };
                        println!("  {symbol} {}: {}", bp.name, bp.plan.description);
                    }
                    let changes = plans.iter().filter(|p| p.plan.action != PlanAction::None).count();
                    println!("\n{changes} block(s) to change.");
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    process::exit(1);
                }
            }
        }
        Command::Apply { target } => {
            let (mut dag, base, store) = load_module(&registry);
            match engine::apply(&mut dag, &base, store.as_ref(), target.as_deref()) {
                Ok(results) => {
                    for bp in &results {
                        let symbol = match bp.plan.action {
                            PlanAction::Create => "+",
                            PlanAction::Update => "~",
                            PlanAction::Replace => "!",
                            PlanAction::Destroy => "-",
                            PlanAction::None => " ",
                        };
                        println!("  {symbol} {}: {}", bp.name, bp.plan.description);
                    }
                    let changes = results.iter().filter(|p| p.plan.action != PlanAction::None).count();
                    println!("\n{changes} block(s) applied.");
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    process::exit(1);
                }
            }
        }
        Command::Destroy { target } => {
            let (mut dag, _base, store) = load_module(&registry);
            match engine::destroy(&mut dag, store.as_ref(), target.as_deref()) {
                Ok(()) => {
                    println!("Destroy complete.");
                }
                Err(e) => {
                    eprintln!("error: {e}");
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
                        Some(doc) => println!("  {name} — {doc}"),
                        None => println!("  {name}"),
                    }
                }
            }
        }
    }
}

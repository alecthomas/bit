use std::fs;
use std::path::Path;
use std::process;

use clap::{Parser, Subcommand};
use yansi::Paint;

use bit::engine::{self, BlockPlan};
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
            eprintln!("{} cannot read BUILD.bit: {e}", "error:".red().bold());
            process::exit(1);
        }
    };
    let module = match bit::parser::parse(&source) {
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

fn print_plans(plans: &[BlockPlan], verb: &str) {
    if plans.is_empty() {
        println!("No blocks to {verb}.");
        return;
    }
    for bp in plans {
        let (symbol, color) = match bp.plan.action {
            PlanAction::Create => ("+", yansi::Color::Green),
            PlanAction::Update => ("~", yansi::Color::Yellow),
            PlanAction::Replace => ("!", yansi::Color::Magenta),
            PlanAction::Destroy => ("-", yansi::Color::Red),
            PlanAction::None => (" ", yansi::Color::Primary),
        };
        let prefix = Paint::paint(&symbol, color).bold();
        let name = Paint::paint(bp.name.as_str(), color).bold();
        println!("  {prefix} {name}: {}", bp.plan.description);
    }
    let changes = plans.iter().filter(|p| p.plan.action != PlanAction::None).count();
    println!("\n{} block(s) {verb}.", changes.to_string().bold());
}

fn main() {
    let cli = Cli::parse();
    let registry = default_registry();

    match cli.command {
        Command::Plan { target } => {
            let (mut dag, base, _store) = load_module(&registry);
            match engine::plan(&mut dag, &base, target.as_deref()) {
                Ok(plans) => print_plans(&plans, "to change"),
                Err(e) => {
                    eprintln!("{} {e}", "error:".red().bold());
                    process::exit(1);
                }
            }
        }
        Command::Apply { target } => {
            let (mut dag, base, store) = load_module(&registry);
            match engine::apply(&mut dag, &base, store.as_ref(), target.as_deref()) {
                Ok(results) => print_plans(&results, "applied"),
                Err(e) => {
                    eprintln!("{} {e}", "error:".red().bold());
                    process::exit(1);
                }
            }
        }
        Command::Destroy { target } => {
            let (mut dag, _base, store) = load_module(&registry);
            // Show what will be destroyed before doing it
            let order = match target.as_deref() {
                Some(t) => dag.target_order(t),
                None => dag.topo_order(),
            };
            match order {
                Ok(mut names) => {
                    names.reverse();
                    for name in &names {
                        if let Some(node) = dag.get_node(name) {
                            if node.protected {
                                println!("  {} {}: {}", " ".bold(), name.bold(), "protected, skipping".dim());
                            } else if node.prior_state.is_some() {
                                println!("  {} {}", "-".red().bold(), name.red().bold());
                            }
                        }
                    }
                    let had_output = names.iter().any(|n| {
                        dag.get_node(n)
                            .is_some_and(|node| node.protected || node.prior_state.is_some())
                    });
                    match engine::destroy(&mut dag, store.as_ref(), target.as_deref()) {
                        Ok(()) => {
                            if had_output {
                                println!();
                            }
                            println!("{}", "Destroy complete.".green());
                        }
                        Err(e) => {
                            eprintln!("{} {e}", "error:".red().bold());
                            process::exit(1);
                        }
                    }
                }
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

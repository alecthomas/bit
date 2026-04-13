use std::fs;
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
    /// Show parameters, targets, and outputs
    #[command(alias = "list")]
    Info,
    /// Dump evaluated inputs and stored outputs
    Dump {
        /// Target to dump (default: all blocks)
        target: Option<String>,
    },
    /// Show provider/resource schema
    Schema {
        /// Provider or provider.resource (e.g. "go", "go.exe", "docker.image")
        filter: Option<String>,
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
        } else if let Ok(n) = val.parse::<bigdecimal::BigDecimal>() {
            bit::value::Value::Number(n)
        } else {
            bit::value::Value::Str(val.to_owned())
        };
        params.insert(key.to_owned(), value);
    }
    params
}

/// Search for BUILD.bit in the current directory and parent directories.
/// Changes to that directory so all relative paths resolve correctly.
/// Exits with an error if not found.
fn find_and_chdir_project_root() {
    let mut dir = std::env::current_dir().unwrap_or_else(|e| {
        eprintln!("{} cannot determine current directory: {e}", "error:".red().bold());
        process::exit(1);
    });
    loop {
        if dir.join("BUILD.bit").is_file() {
            std::env::set_current_dir(&dir).unwrap_or_else(|e| {
                eprintln!("{} cannot chdir to {}: {e}", "error:".red().bold(), dir.display());
                process::exit(1);
            });
            return;
        }
        if !dir.pop() {
            break;
        }
    }
    eprintln!(
        "{} cannot find BUILD.bit in current or parent directories",
        "error:".red().bold()
    );
    process::exit(1);
}

fn load_module(
    registry: &ProviderRegistry,
    params: &Map,
) -> (
    bit::ast::Module,
    bit::dag::Dag,
    loader::BaseScope,
    Box<dyn bit::state::StateStore>,
) {
    let root = std::path::Path::new(".");
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
    let (dag, base) = match loader::load(&module, params, registry, store.as_ref(), root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{} {e}", "error:".red().bold());
            process::exit(1);
        }
    };
    (module, dag, base, store)
}

fn main() {
    let cli = Cli::parse();
    find_and_chdir_project_root();
    let registry = default_registry();
    let params = parse_params(&cli.params);
    let jobs = cli
        .jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));

    let command = cli.command.unwrap_or(Command::Apply { target: None });
    match command {
        Command::Plan { target } => {
            let (_module, mut dag, base, store) = load_module(&registry, &params);
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
            let (_module, mut dag, base, store) = load_module(&registry, &params);
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
            let (_module, mut dag, base, store) = load_module(&registry, &params);
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
            let (_module, mut dag, _base, store) = load_module(&registry, &params);
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
            let (_module, mut dag, base, _store) = load_module(&registry, &params);
            match engine::dump(&mut dag, &base, target.as_deref()) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("{} {e}", "error:".red().bold());
                    process::exit(1);
                }
            }
        }
        Command::Info => {
            use bit::ast::Statement;

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

            let module_params: Vec<_> = module
                .statements
                .iter()
                .filter_map(|s| match s {
                    Statement::Param(p) => Some(p),
                    _ => None,
                })
                .collect();
            let targets: Vec<_> = module
                .statements
                .iter()
                .filter_map(|s| match s {
                    Statement::Target(t) => Some(t),
                    _ => None,
                })
                .collect();
            let outputs: Vec<_> = module
                .statements
                .iter()
                .filter_map(|s| match s {
                    Statement::Output(o) => Some(o),
                    _ => None,
                })
                .collect();

            if !module_params.is_empty() {
                println!("{}:", "Parameters".bold());
                for p in &module_params {
                    let typ = p.typ.to_string();
                    let default = p.default.as_ref().map(|d| format!(" = {d}")).unwrap_or_default();
                    let sig = format!("{typ}{default}");
                    match &p.doc {
                        Some(doc) => println!("  {} ({}) — {}", p.name.bold(), sig.dim(), doc.dim()),
                        None => println!("  {} ({})", p.name.bold(), sig.dim()),
                    }
                }
                println!();
            }

            if !targets.is_empty() {
                println!("{}:", "Targets".bold());
                for t in &targets {
                    match &t.doc {
                        Some(doc) => println!("  {} — {}", t.name.bold(), doc.dim()),
                        None => println!("  {}", t.name.bold()),
                    }
                }
                println!();
            }

            if !outputs.is_empty() {
                println!("{}:", "Outputs".bold());
                for o in &outputs {
                    match &o.doc {
                        Some(doc) => println!("  {} — {}", o.name.bold(), doc.dim()),
                        None => println!("  {}", o.name.bold()),
                    }
                }
                println!();
            }

            if module_params.is_empty() && targets.is_empty() && outputs.is_empty() {
                println!("No parameters, targets, or outputs defined.");
            }
        }
        Command::Schema { filter } => {
            print_schema(&registry, filter.as_deref());
        }
    }
}

fn print_schema(registry: &ProviderRegistry, filter: Option<&str>) {
    // Also scan .bit/modules/ for module providers
    let root = std::path::Path::new(".");
    let module_schemas = scan_module_schemas(root);

    // Collect all matching (display_name, schema) pairs
    let mut entries: Vec<(String, bit::provider::ResourceSchema)> = Vec::new();

    let filter_parts = filter.map(|f| match f.split_once('.') {
        Some((p, r)) => (p, Some(r)),
        None => (f, None),
    });

    // Native providers
    for provider_name in registry.provider_names() {
        if let Some((fp, _)) = filter_parts
            && fp != provider_name
        {
            continue;
        }
        for res in registry.provider_resources(provider_name) {
            if let Some((_, Some(fr))) = filter_parts
                && fr != res.name()
            {
                continue;
            }
            let display = if res.name() == provider_name {
                provider_name.to_owned()
            } else {
                format!("{provider_name}.{}", res.name())
            };
            entries.push((display, res.schema()));
        }
    }

    // Module providers
    for (display_name, mod_resource, schema) in &module_schemas {
        let mod_provider = display_name.split('.').next().unwrap_or(display_name);
        if let Some((fp, _)) = filter_parts
            && fp != mod_provider
        {
            continue;
        }
        if let Some((_, Some(fr))) = filter_parts
            && fr != mod_resource.as_str()
        {
            continue;
        }
        entries.push((display_name.clone(), schema.clone()));
    }

    if entries.is_empty()
        && let Some(f) = filter
    {
        eprintln!("{} unknown provider/resource: {f}", "error:".red().bold());
        process::exit(1);
    }

    for (i, (name, schema)) in entries.iter().enumerate() {
        if i > 0 {
            println!();
        }
        print_resource_schema(name, schema);
    }
}

fn print_resource_schema(name: &str, schema: &bit::provider::ResourceSchema) {
    use bit::provider::ResourceKind;

    let kind_label = match schema.kind {
        ResourceKind::Build => "build",
        ResourceKind::Test => "test",
    };

    println!("{} ({}) — {}", name.bold(), kind_label.dim(), schema.description);

    if !schema.inputs.is_empty() {
        println!("  {}:", "Inputs".bold());
        for f in &schema.inputs {
            let req = if f.required { "" } else { "?" };
            let def = f
                .default
                .as_ref()
                .map(|v| format!(" = {}", v.to_literal()))
                .unwrap_or_default();
            match &f.description {
                Some(desc) => println!(
                    "    {}{} ({}{}) — {}",
                    f.name,
                    req,
                    f.typ.to_string().dim(),
                    def.dim(),
                    desc.dim()
                ),
                None => println!("    {}{} ({}{})", f.name, req, f.typ.to_string().dim(), def.dim()),
            }
        }
    }

    if !schema.outputs.is_empty() {
        println!("  {}:", "Outputs".bold());
        for f in &schema.outputs {
            match &f.description {
                Some(desc) => println!("    {} ({}) — {}", f.name, f.typ.to_string().dim(), desc.dim()),
                None => println!("    {} ({})", f.name, f.typ.to_string().dim()),
            }
        }
    }
}

/// Scan .bit/modules/ for module files and derive their schemas.
fn scan_module_schemas(root: &std::path::Path) -> Vec<(String, String, bit::provider::ResourceSchema)> {
    use bit::provider::{FieldSchema, ResourceKind, ResourceSchema};

    let modules_dir = root.join(".bit/modules");
    let Ok(providers) = std::fs::read_dir(&modules_dir) else {
        return vec![];
    };

    let mut results = Vec::new();
    for provider_entry in providers.flatten() {
        if !provider_entry.path().is_dir() {
            continue;
        }
        let provider_name = provider_entry.file_name().to_string_lossy().into_owned();
        let Ok(resources) = std::fs::read_dir(provider_entry.path()) else {
            continue;
        };
        for res_entry in resources.flatten() {
            let path = res_entry.path();
            let Some(ext) = path.extension() else {
                continue;
            };
            if ext != "bit" {
                continue;
            }
            let resource_name = path.file_stem().unwrap().to_string_lossy().into_owned();
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(module) = bit::parser::parse(&source, &path.display().to_string()) else {
                continue;
            };

            let mut inputs = Vec::new();
            let mut outputs = Vec::new();
            for stmt in &module.statements {
                match stmt {
                    bit::ast::Statement::Param(p) => {
                        let default = p
                            .default
                            .as_ref()
                            .and_then(|d| bit::expr::eval(d, &bit::expr::Scope::new()).ok());
                        inputs.push(FieldSchema {
                            name: p.name.clone(),
                            typ: p.typ.clone(),
                            required: p.default.is_none(),
                            default,
                            description: p.doc.clone(),
                        });
                    }
                    bit::ast::Statement::Output(o) => {
                        outputs.push(FieldSchema {
                            name: o.name.clone(),
                            typ: bit::value::Type::String,
                            required: true,
                            default: None,
                            description: o.doc.clone(),
                        });
                    }
                    _ => {}
                }
            }

            let display_name = if resource_name == provider_name {
                provider_name.clone()
            } else {
                format!("{provider_name}.{resource_name}")
            };

            results.push((
                display_name,
                resource_name,
                ResourceSchema {
                    description: module.doc.unwrap_or_else(|| format!("Module from {}", path.display())),
                    kind: ResourceKind::Build,
                    inputs,
                    outputs,
                },
            ));
        }
    }
    results.sort_by(|a, b| a.0.cmp(&b.0));
    results
}

use std::fs;
use std::process;

use clap::Parser;
use yansi::Paint;

use bit::engine;
use bit::loader;
use bit::output::Output;
use bit::provider::ProviderRegistry;
use bit::providers::docker::DockerProvider;
use bit::providers::exec::ExecProvider;
use bit::providers::go::GoProvider;
use bit::providers::pnpm::PnpmProvider;
use bit::providers::rust::RustProvider;
use bit::state;
use bit::value::Map;

#[derive(Parser)]
#[command(name = "bit", about = "bit — Build It", version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    /// Number of parallel jobs (default: number of CPUs)
    #[arg(short = 'j', long = "jobs")]
    jobs: Option<usize>,

    /// Set a parameter value (e.g. -P verbose=true)
    #[arg(short = 'P', long = "param", value_name = "KEY=VALUE")]
    params: Vec<String>,

    /// Show what would change without applying
    #[arg(short = 'p', long)]
    plan: bool,

    /// Destroy the named blocks and their dependents in reverse topological order
    #[arg(short = 'c', long)]
    clean: bool,

    /// Run test blocks and their dependencies
    #[arg(short = 't', long)]
    test: bool,

    /// Dump evaluated inputs and stored outputs
    #[arg(short = 'd', long)]
    dump: bool,

    /// Show parameters, targets, and outputs
    #[arg(short = 'i', long)]
    info: bool,

    /// List all blocks
    #[arg(short = 'l', long)]
    list: bool,

    /// Render the block dependency graph as ASCII art
    #[arg(short = 'g', long)]
    graph: bool,

    /// Show provider/resource schema (optional filter: "go", "docker.image")
    #[arg(short = 's', long, num_args = 0..=1, default_missing_value = "")]
    schema: Option<String>,

    /// Emit verbose debug information through the output system
    #[arg(short = 'D', long)]
    debug: bool,

    /// Disable live scrolling regions; stream all output line-by-line.
    #[arg(short = 'L', long)]
    long: bool,

    /// Targets or blocks to operate on
    targets: Vec<String>,
}

fn default_registry() -> ProviderRegistry {
    let mut reg = ProviderRegistry::new();
    reg.register(Box::new(ExecProvider));
    reg.register(Box::new(DockerProvider));
    reg.register(Box::new(GoProvider));
    reg.register(Box::new(PnpmProvider));
    reg.register(Box::new(RustProvider));
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
    let store = match state::default_store(root) {
        Ok(s) => Box::new(s),
        Err(e) => {
            eprintln!("{} cannot open state store: {e}", "error:".red().bold());
            process::exit(1);
        }
    };
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

/// Create an Output formatter sized to the blocks that will actually run.
fn make_output(dag: &bit::dag::Dag, targets: &[String], debug: bool, long: bool) -> Output {
    let names = engine::resolve_order(dag, targets).unwrap_or_default();
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    Output::new(&name_refs).with_debug(debug).with_long(long)
}

/// Compute per-node graph styles that reflect each block's planned action.
/// Invokes `engine::plan` with a silent Output so the event stream doesn't
/// pollute stdout before the graph is rendered. Plan errors are returned
/// to the caller so `--plan --graph` surfaces them instead of silently
/// degrading to an unstyled graph.
fn plan_styles(
    dag: &mut bit::dag::Dag,
    base: &bit::loader::BaseScope,
    store: &dyn bit::state::StateStore,
    targets: &[String],
) -> Result<std::collections::HashMap<String, bit::graph::NodeStyle>, engine::EngineError> {
    use yansi::Paint;
    let silent = Output::silent();
    let plans = engine::plan(dag, base, store, &silent, targets)?;
    Ok(plans
        .into_iter()
        .map(|bp| {
            let event = engine::plan_action_to_event(&bp.plan.action);
            let label = bp.name.clone();
            let dim = matches!(event, bit::output::Event::NoChange | bit::output::Event::Protected);
            let paint_with = |text: &str| {
                let painted = text.paint(event.color());
                if dim {
                    painted.dim().to_string()
                } else {
                    painted.to_string()
                }
            };
            let rendered = paint_with(&label);
            // Event::symbol() returns a single-char marker (`+`, `~`, `-`, `·`, `⊘`).
            let symbol = event.symbol();
            let arrow = symbol.chars().next().map(|glyph| (glyph, paint_with(symbol)));
            (bp.name, bit::graph::NodeStyle { label, rendered, arrow })
        })
        .collect())
}

fn main() {
    let cli = Cli::parse();

    // --schema doesn't need the full DAG
    if let Some(ref filter) = cli.schema {
        let registry = default_registry();
        find_and_chdir_project_root();
        let filter = if filter.is_empty() { None } else { Some(filter.as_str()) };
        print_schema(&registry, filter);
        return;
    }

    // --info doesn't need the full DAG
    if cli.info {
        find_and_chdir_project_root();
        print_info();
        return;
    }

    // Validate mutually exclusive mode flags. `--plan` and `--graph` are
    // the one permitted combination: together they render a coloured
    // graph annotated with each block's planned action.
    let exclusive_modes = [cli.clean, cli.test, cli.dump, cli.list];
    let exclusive_count = exclusive_modes.iter().filter(|&&b| b).count();
    let with_plan_or_graph = cli.plan || cli.graph;
    if exclusive_count > 1 || (exclusive_count == 1 && with_plan_or_graph) {
        eprintln!(
            "{} --plan, --clean, --test, --dump, --list, and --graph are mutually exclusive (except --plan --graph)",
            "error:".red().bold()
        );
        process::exit(1);
    }

    find_and_chdir_project_root();
    let registry = default_registry();
    let params = parse_params(&cli.params);
    let jobs = cli
        .jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
    let targets = &cli.targets;

    if cli.graph {
        let (_module, mut dag, base, store) = load_module(&registry, &params);
        let names = match engine::resolve_order(&dag, targets) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("{} {e}", "error:".red().bold());
                process::exit(1);
            }
        };
        let styles = if cli.plan {
            match plan_styles(&mut dag, &base, store.as_ref(), targets) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("{} {e}", "error:".red().bold());
                    process::exit(1);
                }
            }
        } else {
            std::collections::HashMap::new()
        };
        println!("{}", bit::graph::render(&dag, &names, &styles));
    } else if cli.plan {
        let (_module, mut dag, base, store) = load_module(&registry, &params);
        let output = make_output(&dag, targets, cli.debug, cli.long);
        if let Err(e) = engine::plan(&mut dag, &base, store.as_ref(), &output, targets) {
            eprintln!("{} {e}", "error:".red().bold());
            process::exit(1);
        }
    } else if cli.clean {
        let (_module, mut dag, _base, store) = load_module(&registry, &params);
        let output = make_output(&dag, targets, cli.debug, cli.long);
        if let Err(e) = engine::destroy(&mut dag, store.as_ref(), &output, targets) {
            eprintln!("{} {e}", "error:".red().bold());
            process::exit(1);
        }
    } else if cli.test {
        let (_module, mut dag, base, store) = load_module(&registry, &params);
        let names = dag.test_order().unwrap_or_default();
        let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let output = Output::new(&name_refs).with_debug(cli.debug).with_long(cli.long);
        if let Err(e) = engine::test(&mut dag, &base, store.as_ref(), &output, jobs) {
            eprintln!("{} {e}", "error:".red().bold());
            process::exit(1);
        }
    } else if cli.list {
        let (_module, dag, _base, _store) = load_module(&registry, &params);
        match dag.topo_order() {
            Ok(names) => print_block_tree(&dag, &names),
            Err(e) => {
                eprintln!("{} {e}", "error:".red().bold());
                process::exit(1);
            }
        }
    } else if cli.dump {
        let (_module, mut dag, base, _store) = load_module(&registry, &params);
        if let Err(e) = engine::dump(&mut dag, &base, targets) {
            eprintln!("{} {e}", "error:".red().bold());
            process::exit(1);
        }
    } else {
        // Default: apply
        let (_module, mut dag, base, store) = load_module(&registry, &params);
        let output = make_output(&dag, targets, cli.debug, cli.long);
        if let Err(e) = engine::apply(&mut dag, &base, store.as_ref(), &output, targets, jobs) {
            eprintln!("{} {e}", "error:".red().bold());
            process::exit(1);
        }
    }
}

/// Render the DAG as a tree, grouping blocks under their primary parent
/// (content-coupled dep preferred over synthetic ordering edges, ties
/// broken alphabetically). Blocks with additional parents get an
/// `(also: x, y)` suffix so the extra relationships aren't lost.
fn print_block_tree(dag: &bit::dag::Dag, names: &[String]) {
    use std::collections::HashMap;

    let mut children: HashMap<Option<String>, Vec<String>> = HashMap::new();
    for name in names {
        let parent = dag.primary_parent(name);
        children.entry(parent).or_default().push(name.clone());
    }
    for kids in children.values_mut() {
        kids.sort();
    }
    print_block_tree_node(dag, &children, None, 0);
}

fn print_block_tree_node(
    dag: &bit::dag::Dag,
    children: &std::collections::HashMap<Option<String>, Vec<String>>,
    parent: Option<&str>,
    depth: usize,
) {
    let key = parent.map(String::from);
    let Some(kids) = children.get(&key) else { return };
    for name in kids {
        let Some(node) = dag.get_node(name) else { continue };
        let pad = "  ".repeat(depth);
        let typ = format!("{}.{}", node.provider, node.resource_name);
        // Show extra content-coupled parents inline. Phase-edge
        // ordering parents are omitted — they apply to every default
        // block and would just be noise.
        let primary = dag.primary_parent(name);
        let mut others: Vec<String> = dag
            .content_deps(name)
            .into_iter()
            .filter(|p| Some(p.as_str()) != primary.as_deref())
            .collect();
        others.sort();
        let also = if others.is_empty() {
            String::new()
        } else {
            format!(" (also: {})", others.join(", "))
        };
        match &node.doc {
            Some(doc) => {
                // Terse listing: first line only, ellipsis if truncated.
                let mut lines = doc.lines();
                let first = lines.next().unwrap_or("");
                let suffix = if lines.next().is_some() { "…" } else { "" };
                println!("{pad}{name} ({}) — {}{}{also}", typ.dim(), first.dim(), suffix.dim());
            }
            None => println!("{pad}{name} ({}){also}", typ.dim()),
        }
        print_block_tree_node(dag, children, Some(name), depth + 1);
    }
}

fn print_info() {
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

    let desc = schema.inputs.description.as_deref().unwrap_or("");
    println!("{} ({}) — {}", name.bold(), kind_label.dim(), desc);

    if !schema.inputs.fields.is_empty() {
        println!("  {}:", "Inputs".bold());
        for (field_name, f) in &schema.inputs.fields {
            let def = f
                .default
                .as_ref()
                .map(|v| format!(" = {}", v.to_literal()))
                .unwrap_or_default();
            match &f.description {
                Some(desc) => println!(
                    "    {} ({}{}) — {}",
                    field_name,
                    f.typ.to_string().dim(),
                    def.dim(),
                    desc.dim()
                ),
                None => {
                    println!("    {} ({}{})", field_name, f.typ.to_string().dim(), def.dim())
                }
            }
        }
    }

    if !schema.outputs.fields.is_empty() {
        println!("  {}:", "Outputs".bold());
        for (field_name, f) in &schema.outputs.fields {
            match &f.description {
                Some(desc) => {
                    println!("    {} ({}) — {}", field_name, f.typ.to_string().dim(), desc.dim())
                }
                None => println!("    {} ({})", field_name, f.typ.to_string().dim()),
            }
        }
    }
}

/// Scan .bit/modules/ for module files and derive their schemas.
fn scan_module_schemas(root: &std::path::Path) -> Vec<(String, String, bit::provider::ResourceSchema)> {
    use bit::provider::{ResourceKind, ResourceSchema, StructField, StructType};

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
                        let typ = if p.default.is_some() {
                            bit::value::Type::Optional(Box::new(p.typ.clone()))
                        } else {
                            p.typ.clone()
                        };
                        inputs.push((
                            p.name.clone(),
                            StructField {
                                typ,
                                default,
                                description: p.doc.clone(),
                            },
                        ));
                    }
                    bit::ast::Statement::Output(o) => {
                        outputs.push((
                            o.name.clone(),
                            StructField {
                                typ: bit::value::Type::String,
                                default: None,
                                description: o.doc.clone(),
                            },
                        ));
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
                    kind: ResourceKind::Build,
                    inputs: StructType {
                        description: module.doc.or_else(|| Some(format!("Module from {}", path.display()))),
                        fields: inputs,
                    },
                    outputs: StructType {
                        description: None,
                        fields: outputs,
                    },
                },
            ));
        }
    }
    results.sort_by(|a, b| a.0.cmp(&b.0));
    results
}

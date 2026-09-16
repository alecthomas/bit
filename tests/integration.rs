use std::fs;

use bit::cache::BuildCache;
use bit::engine;
use bit::loader;
use std::sync::{Arc, Mutex};

use bit::file_tracker::FileTracker;
use bit::output::Output;
use bit::parser;
use bit::provider::ProviderRegistry;
use bit::providers::exec::ExecProvider;
use bit::state::{StateError, StateStore};
use bit::value::Map;

fn test_tracker() -> Arc<Mutex<FileTracker>> {
    Arc::new(Mutex::new(FileTracker::new()))
}

fn local_cache() -> BuildCache {
    BuildCache::local_only(std::path::Path::new("."))
}

fn registry(tracker: &Arc<Mutex<FileTracker>>) -> ProviderRegistry {
    let mut reg = ProviderRegistry::new();
    reg.register(Box::new(ExecProvider::new(tracker.clone())));
    reg
}

struct MemoryStore {
    data: std::sync::RwLock<std::collections::HashMap<String, serde_json::Value>>,
}

impl MemoryStore {
    fn new() -> Self {
        Self {
            data: std::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }
}

impl StateStore for MemoryStore {
    fn load(&self, block: &str) -> Result<Option<serde_json::Value>, StateError> {
        Ok(self.data.read().unwrap().get(block).cloned())
    }
    fn save(&self, block: &str, state: &serde_json::Value) -> Result<(), StateError> {
        self.data.write().unwrap().insert(block.into(), state.clone());
        Ok(())
    }
    fn remove(&self, block: &str) -> Result<(), StateError> {
        self.data.write().unwrap().remove(block);
        Ok(())
    }
    fn list(&self) -> Result<Vec<String>, StateError> {
        Ok(self.data.read().unwrap().keys().cloned().collect())
    }
}

fn run_apply(input: &str, store: &MemoryStore) -> Vec<engine::BlockPlan> {
    let tracker = test_tracker();
    let module = parser::parse(input, "<test>").expect("parse failed");
    let (mut dag, base) = loader::load(&module, &Map::new(), &registry(&tracker), store, &[]).expect("load failed");
    engine::apply(
        &mut dag,
        &base,
        store,
        &local_cache(),
        &Output::new(&[]),
        &[],
        1,
        &tracker,
    )
    .expect("apply failed")
}

fn run_plan(input: &str, store: &MemoryStore) -> Vec<engine::BlockPlan> {
    let tracker = test_tracker();
    let module = parser::parse(input, "<test>").expect("parse failed");
    let (mut dag, base) = loader::load(&module, &Map::new(), &registry(&tracker), store, &[]).expect("load failed");
    engine::plan(&mut dag, &base, &local_cache(), &Output::new(&[]), &[], &tracker).expect("plan failed")
}

fn run_dump(input: &str, store: &MemoryStore, targets: &[String]) {
    let tracker = test_tracker();
    let module = parser::parse(input, "<test>").expect("parse failed");
    let (mut dag, base) = loader::load(&module, &Map::new(), &registry(&tracker), store, &[]).expect("load failed");
    engine::dump(&mut dag, &base, targets).expect("dump failed");
}

fn run_destroy(input: &str, store: &MemoryStore) {
    let tracker = test_tracker();
    let module = parser::parse(input, "<test>").expect("parse failed");
    let (mut dag, _base) = loader::load(&module, &Map::new(), &registry(&tracker), store, &[]).expect("load failed");
    engine::destroy(&mut dag, store, &Output::new(&[]), &[], false).expect("destroy failed");
}

#[test]
fn single_exec_block() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("hello.txt");
    let input = format!(
        "hello = exec {{\n  command = \"echo hello > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
        out.display(),
        out.display(),
    );
    let store = MemoryStore::new();
    let results = run_apply(&input, &store);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].plan.action, bit::provider::PlanAction::Create);
    assert!(out.exists());
    assert_eq!(fs::read_to_string(&out).unwrap().trim(), "hello");
}

#[test]
fn chained_blocks_with_refs() {
    let dir = tempfile::tempdir().unwrap();
    let file_a = dir.path().join("a.txt");
    let file_b = dir.path().join("b.txt");
    let input = format!(
        concat!(
            "a = exec {{\n  command = \"echo hello > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            "b = exec {{\n  command = \"cp #{{a.path}} {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
        ),
        file_a.display(),
        file_a.display(),
        file_b.display(),
        file_b.display(),
    );
    let store = MemoryStore::new();
    let results = run_apply(&input, &store);
    assert_eq!(results.len(), 2);
    assert!(file_a.exists());
    // b depends on a.path which should resolve to the output path
}

#[test]
fn plan_then_apply() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.txt");
    let input = format!(
        "build = exec {{\n  command = \"echo built > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
        out.display(),
        out.display(),
    );
    let store = MemoryStore::new();

    let plans = run_plan(&input, &store);
    assert_eq!(plans[0].plan.action, bit::provider::PlanAction::Create);
    assert!(!out.exists());

    let results = run_apply(&input, &store);
    assert_eq!(results[0].plan.action, bit::provider::PlanAction::Create);
    assert!(out.exists());
}

#[test]
fn second_apply_is_noop() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.txt");
    let input = format!(
        "build = exec {{\n  command = \"echo built > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
        out.display(),
        out.display(),
    );
    let store = MemoryStore::new();

    run_apply(&input, &store);
    let results = run_apply(&input, &store);
    assert_eq!(results[0].plan.action, bit::provider::PlanAction::None);
}

#[test]
fn destroy_removes_state() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.txt");
    let input = format!(
        "build = exec {{\n  command = \"echo hi > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
        out.display(),
        out.display(),
    );
    let store = MemoryStore::new();

    run_apply(&input, &store);
    assert!(!store.list().unwrap().is_empty());

    run_destroy(&input, &store);
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn explicit_block_excluded_from_default_apply() {
    let dir = tempfile::tempdir().unwrap();
    let out_a = dir.path().join("a.txt");
    let out_b = dir.path().join("b.txt");
    let input = format!(
        concat!(
            "a = exec {{\n  command = \"echo a > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            "explicit b = exec {{\n  command = \"echo b > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
        ),
        out_a.display(),
        out_a.display(),
        out_b.display(),
        out_b.display(),
    );
    let store = MemoryStore::new();

    // Default apply (no targets, no `default` target) skips the explicit block.
    run_apply(&input, &store);
    assert!(out_a.exists());
    assert!(!out_b.exists());

    // Naming it explicitly runs it.
    let tracker = test_tracker();
    let module = parser::parse(&input, "<test>").unwrap();
    let (mut dag, base) = loader::load(&module, &Map::new(), &registry(&tracker), &store, &[]).unwrap();
    engine::apply(
        &mut dag,
        &base,
        &store,
        &local_cache(),
        &Output::new(&[]),
        &["b".into()],
        1,
        &tracker,
    )
    .unwrap();
    assert!(out_b.exists());
}

#[test]
fn protected_block_survives_destroy() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.txt");
    let input = format!(
        "protected build = exec {{\n  command = \"echo hi > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
        out.display(),
        out.display(),
    );
    let store = MemoryStore::new();

    run_apply(&input, &store);
    run_destroy(&input, &store);
    assert!(!store.list().unwrap().is_empty());
}

#[test]
fn target_filters_execution() {
    let dir = tempfile::tempdir().unwrap();
    let out_a = dir.path().join("a.txt");
    let out_b = dir.path().join("b.txt");
    let input = format!(
        concat!(
            "a = exec {{\n  command = \"echo a > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            "b = exec {{\n  command = \"echo b > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            "target just_a = [a]\n",
        ),
        out_a.display(),
        out_a.display(),
        out_b.display(),
        out_b.display(),
    );
    let store = MemoryStore::new();
    let tracker = test_tracker();
    let module = parser::parse(&input, "<test>").unwrap();
    let (mut dag, base) = loader::load(&module, &Map::new(), &registry(&tracker), &store, &[]).unwrap();
    let results = engine::apply(
        &mut dag,
        &base,
        &store,
        &local_cache(),
        &Output::new(&[]),
        &["just_a".into()],
        1,
        &tracker,
    )
    .unwrap();
    assert_eq!(results.len(), 1);
    assert!(out_a.exists());
    assert!(!out_b.exists());
}

#[test]
fn let_bindings_in_block_fields() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.txt");
    let input = format!(
        concat!(
            "let msg = \"hello world\"\n",
            "build = exec {{\n  command = \"echo #{{msg}} > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
        ),
        out.display(),
        out.display(),
    );
    let store = MemoryStore::new();
    run_apply(&input, &store);
    assert_eq!(fs::read_to_string(&out).unwrap().trim(), "hello world");
}

#[test]
fn params_with_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.txt");
    let input = format!(
        concat!(
            "param msg : string = \"default\"\n",
            "build = exec {{\n  command = \"echo #{{msg}} > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
        ),
        out.display(),
        out.display(),
    );
    let store = MemoryStore::new();
    run_apply(&input, &store);
    assert_eq!(fs::read_to_string(&out).unwrap().trim(), "default");
}

#[test]
fn pipe_in_let_binding() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.txt");
    let input = format!(
        concat!(
            "let sha = exec(\"echo abc123\") | trim\n",
            "build = exec {{\n  command = \"echo #{{sha}} > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
        ),
        out.display(),
        out.display(),
    );
    let store = MemoryStore::new();
    run_apply(&input, &store);
    assert_eq!(fs::read_to_string(&out).unwrap().trim(), "abc123");
}

#[test]
fn diamond_dependency() {
    let dir = tempfile::tempdir().unwrap();
    let out_a = dir.path().join("a.txt");
    let out_b = dir.path().join("b.txt");
    let out_c = dir.path().join("c.txt");
    let out_d = dir.path().join("d.txt");
    let input = format!(
        concat!(
            "a = exec {{\n  command = \"echo a > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            "b = exec {{\n  command = \"echo b #{{a.path}} > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            "c = exec {{\n  command = \"echo c #{{a.path}} > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            "d = exec {{\n  command = \"echo d #{{b.path}} #{{c.path}} > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
        ),
        out_a.display(),
        out_a.display(),
        out_b.display(),
        out_b.display(),
        out_c.display(),
        out_c.display(),
        out_d.display(),
        out_d.display(),
    );
    let store = MemoryStore::new();
    let results = run_apply(&input, &store);
    assert_eq!(results.len(), 4);
    assert!(out_a.exists());
    assert!(out_b.exists());
    assert!(out_c.exists());
    assert!(out_d.exists());
}

#[test]
fn dependency_change_propagates_to_plan() {
    let dir = tempfile::tempdir().unwrap();
    let out_a = dir.path().join("a.txt");
    let out_b = dir.path().join("b.txt");

    // Apply both blocks
    let input_v1 = format!(
        concat!(
            "a = exec {{\n  command = \"echo v1 > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            "b = exec {{\n  command = \"echo ok > {}\"\n  output = \"{}\"\n  inputs = []\n  depends_on = [a]\n}}\n",
        ),
        out_a.display(),
        out_a.display(),
        out_b.display(),
        out_b.display(),
    );
    let store = MemoryStore::new();
    run_apply(&input_v1, &store);

    // Second plan is noop
    let plans = run_plan(&input_v1, &store);
    assert_eq!(
        plans[0].plan.action,
        bit::provider::PlanAction::None,
        "a should be unchanged"
    );
    assert_eq!(
        plans[1].plan.action,
        bit::provider::PlanAction::None,
        "b should be unchanged"
    );

    // Change a's command (simulating a source change)
    let input_v2 = format!(
        concat!(
            "a = exec {{\n  command = \"echo v2 > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            "b = exec {{\n  command = \"echo ok > {}\"\n  output = \"{}\"\n  inputs = []\n  depends_on = [a]\n}}\n",
        ),
        out_a.display(),
        out_a.display(),
        out_b.display(),
        out_b.display(),
    );

    // Plan should show a as Update and b as Update (dependencies changed)
    let plans = run_plan(&input_v2, &store);
    assert_eq!(
        plans[0].plan.action,
        bit::provider::PlanAction::Update,
        "a should need update"
    );
    assert_eq!(
        plans[1].plan.action,
        bit::provider::PlanAction::Update,
        "b should need update due to dependency"
    );

    // Apply a only, then plan should still show b as needing update (cross-run)
    let input_a_only = format!(
        "a = exec {{\n  command = \"echo v2 > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
        out_a.display(),
        out_a.display(),
    );
    run_apply(&input_a_only, &store);

    // Now plan the full config — a is clean but b's dep hash should differ
    let plans = run_plan(&input_v2, &store);
    assert_eq!(
        plans[0].plan.action,
        bit::provider::PlanAction::None,
        "a should be clean after apply"
    );
    assert_eq!(
        plans[1].plan.action,
        bit::provider::PlanAction::Update,
        "b should still need update (dep hash changed)"
    );
}

#[test]
fn after_does_not_propagate_changes() {
    let dir = tempfile::tempdir().unwrap();
    let out_a = dir.path().join("a.txt");
    let out_b = dir.path().join("b.txt");

    // Apply both blocks — b runs after a but is not content-coupled
    let input_v1 = format!(
        concat!(
            "a = exec {{\n  command = \"echo v1 > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            "b = exec {{\n  command = \"echo ok > {}\"\n  output = \"{}\"\n  inputs = []\n  after = [a]\n}}\n",
        ),
        out_a.display(),
        out_a.display(),
        out_b.display(),
        out_b.display(),
    );
    let store = MemoryStore::new();
    run_apply(&input_v1, &store);

    // Change a's command
    let input_v2 = format!(
        concat!(
            "a = exec {{\n  command = \"echo v2 > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            "b = exec {{\n  command = \"echo ok > {}\"\n  output = \"{}\"\n  inputs = []\n  after = [a]\n}}\n",
        ),
        out_a.display(),
        out_a.display(),
        out_b.display(),
        out_b.display(),
    );

    // Plan should show a as Update but b as None (after is ordering-only)
    let plans = run_plan(&input_v2, &store);
    assert_eq!(
        plans[0].plan.action,
        bit::provider::PlanAction::Update,
        "a should need update"
    );
    assert_eq!(
        plans[1].plan.action,
        bit::provider::PlanAction::None,
        "b should not be affected by a (after is ordering-only)"
    );
}

#[test]
fn doc_comments_preserved() {
    let input = concat!(
        "# The server\n",
        "server = exec {\n  command = \"echo hi\"\n  output = \"out\"\n  inputs = []\n}\n",
        "# Build everything\n",
        "target build = [server]\n",
    );
    let tracker = test_tracker();
    let module = parser::parse(input, "<test>").unwrap();
    let store = MemoryStore::new();
    let (dag, _base) = loader::load(&module, &Map::new(), &registry(&tracker), &store, &[]).unwrap();
    let node = dag.get_node("server").unwrap();
    assert_eq!(node.fields.len(), 3);
    let targets = dag.targets();
    assert_eq!(targets["build"].doc.as_deref(), Some("Build everything"));
}

#[test]
fn dump_before_apply() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.txt");
    let input = format!(
        "build = exec {{\n  command = \"echo hi > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
        out.display(),
        out.display(),
    );
    let store = MemoryStore::new();
    // Dump should succeed even with no prior state
    run_dump(&input, &store, &[]);
}

#[test]
fn dump_after_apply() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.txt");
    let input = format!(
        "build = exec {{\n  command = \"echo hi > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
        out.display(),
        out.display(),
    );
    let store = MemoryStore::new();
    run_apply(&input, &store);
    // Dump should show both inputs and stored outputs
    run_dump(&input, &store, &[]);
}

#[test]
fn dump_with_target() {
    let dir = tempfile::tempdir().unwrap();
    let out_a = dir.path().join("a.txt");
    let out_b = dir.path().join("b.txt");
    let input = format!(
        concat!(
            "a = exec {{\n  command = \"echo a > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            "b = exec {{\n  command = \"echo b > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            "target just_a = [a]\n",
        ),
        out_a.display(),
        out_a.display(),
        out_b.display(),
        out_b.display(),
    );
    let store = MemoryStore::new();
    run_apply(&input, &store);
    // Dump filtered to target should succeed
    run_dump(&input, &store, &["just_a".into()]);
}

/// Helper to set up a module file at {dir}/{provider}/{resource}.bit
fn write_module(dir: &std::path::Path, provider: &str, resource: &str, content: &str) {
    let module_dir = dir.join(provider);
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(module_dir.join(format!("{resource}.bit")), content).unwrap();
}

fn run_apply_in_dir(dir: &std::path::Path, input: &str, store: &MemoryStore) -> Vec<engine::BlockPlan> {
    let tracker = test_tracker();
    let module = parser::parse(input, "<test>").expect("parse failed");
    // Module-system tests in this file all use the `mymod` provider, mirroring
    // the per-import-equals-one-provider layout.
    let import_roots = vec![bit::import::ImportRoot {
        provider: "mymod".into(),
        path: dir.join("mymod"),
    }];
    let (mut dag, base) =
        loader::load(&module, &Map::new(), &registry(&tracker), store, &import_roots).expect("load failed");
    engine::apply(
        &mut dag,
        &base,
        store,
        &local_cache(),
        &Output::new(&[]),
        &[],
        1,
        &tracker,
    )
    .expect("apply failed")
}

#[test]
fn module_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let out_inner = dir.path().join("inner_out.txt");

    write_module(
        dir.path(),
        "mymod",
        "mymod",
        &format!(
            concat!(
                "param msg : string\n",
                "inner = exec {{\n",
                "  command = \"echo #{{msg}} > {}\"\n",
                "  output = \"{}\"\n",
                "  inputs = []\n",
                "}}\n",
                "output result = inner.path\n",
            ),
            out_inner.display(),
            out_inner.display(),
        ),
    );

    let input = r#"
inst = mymod {
  msg = "hello from module"
}
"#;
    let store = MemoryStore::new();
    let results = run_apply_in_dir(dir.path(), input, &store);

    // Should have 2 blocks: inst.inner (exec) and inst (module outputs)
    assert_eq!(results.len(), 2);
    assert!(out_inner.exists());
    assert_eq!(fs::read_to_string(&out_inner).unwrap().trim(), "hello from module");
}

#[test]
fn module_output_forwarding() {
    let dir = tempfile::tempdir().unwrap();
    let out_inner = dir.path().join("mod_out.txt");
    let out_consumer = dir.path().join("consumer_out.txt");

    write_module(
        dir.path(),
        "mymod",
        "mymod",
        &format!(
            concat!(
                "param msg : string\n",
                "inner = exec {{\n",
                "  command = \"echo #{{msg}} > {}\"\n",
                "  output = \"{}\"\n",
                "  inputs = []\n",
                "}}\n",
                "output result = inner.path\n",
            ),
            out_inner.display(),
            out_inner.display(),
        ),
    );

    let input = format!(
        concat!(
            "inst = mymod {{\n",
            "  msg = \"from module\"\n",
            "}}\n",
            "consumer = exec {{\n",
            "  command = \"cp #{{inst.result}} {}\"\n",
            "  output = \"{}\"\n",
            "  inputs = []\n",
            "}}\n",
        ),
        out_consumer.display(),
        out_consumer.display(),
    );
    let store = MemoryStore::new();
    let results = run_apply_in_dir(dir.path(), &input, &store);

    // 3 blocks: inst.inner, inst, consumer
    assert_eq!(results.len(), 3);
    assert!(out_consumer.exists());
    assert_eq!(
        fs::read_to_string(&out_consumer).unwrap().trim(),
        fs::read_to_string(&out_inner).unwrap().trim(),
    );
}

#[test]
fn module_multiple_instances() {
    let dir = tempfile::tempdir().unwrap();
    let out1 = dir.path().join("out1.txt");
    let out2 = dir.path().join("out2.txt");

    write_module(
        dir.path(),
        "mymod",
        "mymod",
        "param msg : string\nparam outfile : string\n\
         inner = exec {\n  command = \"echo #{msg} > #{outfile}\"\n  output = outfile\n  inputs = []\n}\n\
         output result = inner.path\n",
    );

    let input = format!(
        concat!(
            "a = mymod {{\n  msg = \"alpha\"\n  outfile = \"{}\"\n}}\n",
            "b = mymod {{\n  msg = \"beta\"\n  outfile = \"{}\"\n}}\n",
        ),
        out1.display(),
        out2.display(),
    );
    let store = MemoryStore::new();
    let results = run_apply_in_dir(dir.path(), &input, &store);

    assert_eq!(results.len(), 4); // a.inner, a, b.inner, b
    assert_eq!(fs::read_to_string(&out1).unwrap().trim(), "alpha");
    assert_eq!(fs::read_to_string(&out2).unwrap().trim(), "beta");
}

#[test]
fn matrix_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let out_amd64 = dir.path().join("out-amd64.txt");
    let out_arm64 = dir.path().join("out-arm64.txt");

    let input = format!(
        concat!(
            "let arch = [\"amd64\", \"arm64\"]\n",
            "build[arch] = exec {{\n",
            "  command = \"echo #{{arch}} > {dir}/out-#{{arch}}.txt\"\n",
            "  output = \"{dir}/out-#{{arch}}.txt\"\n",
            "  inputs = []\n",
            "}}\n",
        ),
        dir = dir.path().display(),
    );
    let store = MemoryStore::new();
    let results = run_apply(&input, &store);

    assert_eq!(results.len(), 2);
    assert!(out_amd64.exists());
    assert!(out_arm64.exists());
    assert_eq!(fs::read_to_string(&out_amd64).unwrap().trim(), "amd64");
    assert_eq!(fs::read_to_string(&out_arm64).unwrap().trim(), "arm64");
}

// ── Shared build cache: go.exe acceptance ────────────────────────────────

/// Build a Go binary in worktree A, delete A, and confirm worktree B
/// restores the binary from the shared cache without running `go build`.
/// Skipped when `go` is not installed.
#[test]
fn go_exe_restores_from_shared_cache_after_worktree_deleted() {
    use std::process::Command;

    if Command::new("go").arg("version").output().is_err() {
        eprintln!("go not available; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let cache_dir = base.join("cache");

    // Linked worktrees of one repository share receipts.
    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }
    let repo = base.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(
        &repo,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
    );

    fn write_project(root: &std::path::Path) {
        fs::write(root.join("go.mod"), "module example.com/app\n").unwrap();
        fs::write(
            root.join("main.go"),
            "package main\n\nimport \"fmt\"\n\nfunc main() { fmt.Println(\"hello from cache\") }\n",
        )
        .unwrap();
    }

    fn build_bit(root: &std::path::Path) -> String {
        format!(
            "app = go.exe {{\n  package = \".\"\n  dir = \"{}\"\n  output = \"{}\"\n}}\n",
            root.display(),
            root.join("bin/app").display()
        )
    }

    fn run(
        root: &std::path::Path,
        cache_dir: &std::path::Path,
        store: &MemoryStore,
        plan_only: bool,
    ) -> Vec<engine::BlockPlan> {
        let tracker = test_tracker();
        let mut reg = ProviderRegistry::new();
        reg.register(Box::new(bit::providers::go::GoProvider::new(tracker.clone())));
        let module = parser::parse(&build_bit(root), "<test>").unwrap();
        let (mut dag, base) = loader::load(&module, &Map::new(), &reg, store, &[]).unwrap();
        let cache = BuildCache::open_at(root, cache_dir);
        let output = Output::new(&[]);
        if plan_only {
            engine::plan(&mut dag, &base, &cache, &output, &[], &tracker).unwrap()
        } else {
            engine::apply(&mut dag, &base, store, &cache, &output, &[], 1, &tracker).unwrap()
        }
    }

    let a = base.join("a");
    git(&repo, &["worktree", "add", "-q", "--detach", a.to_str().unwrap()]);
    write_project(&a);
    let store_a = MemoryStore::new();
    let plans = run(&a, &cache_dir, &store_a, false);
    assert_eq!(plans[0].plan.action, bit::provider::PlanAction::Create);
    assert!(a.join("bin/app").is_file());
    fs::remove_dir_all(&a).unwrap();

    let b = base.join("b");
    git(&repo, &["worktree", "add", "-q", "--detach", b.to_str().unwrap()]);
    write_project(&b);
    let store_b = MemoryStore::new();
    let plans = run(&b, &cache_dir, &store_b, true);
    assert_eq!(plans[0].plan.action, bit::provider::PlanAction::Restore);
    assert!(!b.join("bin/app").exists(), "plan must not materialize");

    let plans = run(&b, &cache_dir, &store_b, false);
    assert_eq!(plans[0].plan.action, bit::provider::PlanAction::Restore);
    let exe = b.join("bin/app");
    assert!(exe.is_file());
    let out = Command::new(&exe).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello from cache");

    let plans = run(&b, &cache_dir, &store_b, false);
    assert_eq!(plans[0].plan.action, bit::provider::PlanAction::None);
}

/// End to end with the real binary: build a `rust.exe` block in linked
/// worktree A, delete A, and confirm worktree B restores the binary from the
/// shared cache without cargo compiling anything. The binary prints
/// `file!()`, which must be the remapped main-worktree path.
#[test]
fn rust_exe_restores_from_shared_cache_with_remapped_paths() {
    use std::process::Command;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }
    fn write_project(root: &std::path::Path) {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(root.join("src/main.rs"), "fn main() { println!(\"{}\", file!()); }\n").unwrap();
        fs::write(root.join("BUILD.bit"), "app = rust.exe {}\n").unwrap();
    }
    /// Whether any regular file under `dir` contains `needle`.
    fn artifacts_contain(dir: &std::path::Path, needle: &[u8]) -> bool {
        fn walk(dir: &std::path::Path, needle: &[u8]) -> bool {
            let Ok(entries) = fs::read_dir(dir) else { return false };
            entries.flatten().any(|e| {
                let path = e.path();
                if path.is_dir() {
                    walk(&path, needle)
                } else {
                    fs::read(&path).is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
                }
            })
        }
        walk(dir, needle)
    }
    fn bit(root: &std::path::Path, cache: &std::path::Path, args: &[&str]) -> std::process::Output {
        // When this suite itself runs under `bit --test`, cargo hands the
        // test binary the outer worktree's wrapper; the inner bit must see a
        // clean environment or it would defer to that wrapper.
        let out = Command::new(env!("CARGO_BIN_EXE_bit"))
            .args(args)
            .current_dir(root)
            .env("BIT_CACHE_DIR", cache)
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .env_remove("RUSTC_WRAPPER")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "bit {args:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let cache = base.join("cache");
    let repo = base.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(
        &repo,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
    );

    let a = base.join("a");
    git(&repo, &["worktree", "add", "-q", "--detach", a.to_str().unwrap()]);
    write_project(&a);
    bit(&a, &cache, &[]);
    let exe_a = a.join("target/debug/app");
    assert!(exe_a.is_file());
    assert_eq!(
        String::from_utf8_lossy(&Command::new(&exe_a).output().unwrap().stdout).trim(),
        "src/main.rs"
    );
    // `--remap-path-prefix` rewrites the DWARF compilation directory from the
    // worktree to the main repository path, which is otherwise unknown to
    // cargo, so its presence in the build artifacts proves the flag applied.
    assert!(
        artifacts_contain(&a.join("target/debug"), repo.as_os_str().as_encoded_bytes()),
        "build artifacts must embed the remapped main-worktree path"
    );
    fs::remove_dir_all(&a).unwrap();

    let b = base.join("b");
    git(&repo, &["worktree", "add", "-q", "--detach", b.to_str().unwrap()]);
    write_project(&b);
    bit(&b, &cache, &["--plan"]);
    assert!(!b.join("target").exists(), "plan must not materialize");

    bit(&b, &cache, &[]);
    let exe_b = b.join("target/debug/app");
    assert!(exe_b.is_file());
    assert!(
        !b.join("target/debug/.fingerprint").exists(),
        "cargo must not have compiled anything in B"
    );
    assert_eq!(
        String::from_utf8_lossy(&Command::new(&exe_b).output().unwrap().stdout).trim(),
        "src/main.rs"
    );
}

#[test]
fn cache_flag_reports_and_cleans_shared_cache() {
    use std::process::Command;
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache");
    fs::create_dir_all(cache.join("cas/v1/sha256/ab")).unwrap();
    fs::write(cache.join("cas/v1/sha256/ab/abcd"), b"blob").unwrap();
    fs::create_dir_all(cache.join("actions/v1/p")).unwrap();
    fs::write(cache.join("actions/v1/p/k.json"), b"{}").unwrap();
    fs::create_dir_all(cache.join("statehash")).unwrap();
    fs::write(cache.join("statehash/state.json"), b"{}").unwrap();

    let bit = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_bit"))
            .args(args)
            .current_dir(tmp.path())
            .env("BIT_CACHE_DIR", &cache)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let stats = bit(&["--cache"]);
    assert!(stats.contains("receipts: 1 ("), "{stats}");
    assert!(stats.contains("artifacts: 1 (4 B)"), "{stats}");

    bit(&["--cache", "--clean"]);
    assert!(!cache.join("cas").exists());
    assert!(!cache.join("actions").exists());
    assert!(cache.join("statehash/state.json").is_file(), "local state must survive");
    assert!(bit(&["--cache"]).contains("artifacts: 0 (0 B)"));
}

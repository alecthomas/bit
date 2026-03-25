use std::fs;

use bit::engine;
use bit::loader;
use bit::parser;
use bit::provider::ProviderRegistry;
use bit::providers::exec::ExecProvider;
use bit::state::{StateError, StateStore};
use bit::value::Map;

fn registry() -> ProviderRegistry {
    let mut reg = ProviderRegistry::new();
    reg.register(Box::new(ExecProvider));
    reg
}

struct MemoryStore {
    data: std::cell::RefCell<std::collections::HashMap<String, serde_json::Value>>,
}

impl MemoryStore {
    fn new() -> Self {
        Self {
            data: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }
}

impl StateStore for MemoryStore {
    fn load(&self, block: &str) -> Result<Option<serde_json::Value>, StateError> {
        Ok(self.data.borrow().get(block).cloned())
    }
    fn save(&self, block: &str, state: &serde_json::Value) -> Result<(), StateError> {
        self.data.borrow_mut().insert(block.into(), state.clone());
        Ok(())
    }
    fn remove(&self, block: &str) -> Result<(), StateError> {
        self.data.borrow_mut().remove(block);
        Ok(())
    }
    fn list(&self) -> Result<Vec<String>, StateError> {
        Ok(self.data.borrow().keys().cloned().collect())
    }
}

fn run_apply(input: &str, store: &MemoryStore) -> Vec<engine::BlockPlan> {
    let module = parser::parse(input).expect("parse failed");
    let (mut dag, base) = loader::load(&module, &Map::new(), &registry(), store).expect("load failed");
    engine::apply(&mut dag, &base, store, None).expect("apply failed")
}

fn run_plan(input: &str, store: &MemoryStore) -> Vec<engine::BlockPlan> {
    let module = parser::parse(input).expect("parse failed");
    let (mut dag, base) = loader::load(&module, &Map::new(), &registry(), store).expect("load failed");
    engine::plan(&mut dag, &base, None).expect("plan failed")
}

fn run_destroy(input: &str, store: &MemoryStore) {
    let module = parser::parse(input).expect("parse failed");
    let (mut dag, _base) = loader::load(&module, &Map::new(), &registry(), store).expect("load failed");
    engine::destroy(&mut dag, store, None).expect("destroy failed");
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
            "b = exec {{\n  command = \"cp ${{a.path}} {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
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
    let module = parser::parse(&input).unwrap();
    let (mut dag, base) = loader::load(&module, &Map::new(), &registry(), &store).unwrap();
    let results = engine::apply(&mut dag, &base, &store, Some("just_a")).unwrap();
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
            "build = exec {{\n  command = \"echo ${{msg}} > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
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
            "build = exec {{\n  command = \"echo ${{msg}} > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
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
            "build = exec {{\n  command = \"echo ${{sha}} > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
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
            "b = exec {{\n  command = \"echo b ${{a.path}} > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            "c = exec {{\n  command = \"echo c ${{a.path}} > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
            "d = exec {{\n  command = \"echo d ${{b.path}} ${{c.path}} > {}\"\n  output = \"{}\"\n  inputs = []\n}}\n",
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
fn doc_comments_preserved() {
    let input = concat!(
        "# The server\n",
        "server = exec {\n  command = \"echo hi\"\n  output = \"out\"\n  inputs = []\n}\n",
        "# Build everything\n",
        "target build = [server]\n",
    );
    let module = parser::parse(input).unwrap();
    let store = MemoryStore::new();
    let (dag, _base) = loader::load(&module, &Map::new(), &registry(), &store).unwrap();
    let node = dag.get_node("server").unwrap();
    assert_eq!(node.fields.len(), 3);
    let targets = dag.targets();
    assert_eq!(targets["build"].doc.as_deref(), Some("Build everything"));
}

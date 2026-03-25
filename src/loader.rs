use std::collections::HashMap;

use crate::ast::{Module, Statement};
use crate::expr::{self, EvalError, Scope};
use crate::value::{Map, Value};

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("eval error: {0}")]
    Eval(#[from] EvalError),
    #[error("missing required param: {0}")]
    MissingParam(String),
    #[error("duplicate name: {0}")]
    Duplicate(String),
}

/// A loaded block ready for the engine.
#[derive(Debug, Clone)]
pub struct LoadedBlock {
    pub name: String,
    pub provider: String,
    pub resource: String,
    pub protected: bool,
    pub inputs: Map,
}

/// Result of loading a module.
#[derive(Debug)]
pub struct LoadedModule {
    pub blocks: Vec<LoadedBlock>,
    pub targets: HashMap<String, Vec<String>>,
    pub outputs: HashMap<String, Value>,
}

/// Load a parsed module: evaluate lets, resolve params, evaluate block fields.
///
/// `params` supplies values for declared parameters. Params with defaults
/// fall back to those defaults if not supplied.
pub fn load(module: &Module, params: &Map) -> Result<LoadedModule, LoadError> {
    let mut scope = Scope::new();
    let mut blocks = Vec::new();
    let mut targets = HashMap::new();
    let mut outputs = HashMap::new();

    for stmt in &module.statements {
        match stmt {
            Statement::Param(p) => {
                let value = if let Some(v) = params.get(&p.name) {
                    v.clone()
                } else if let Some(default) = &p.default {
                    expr::eval(default, &scope)?
                } else {
                    return Err(LoadError::MissingParam(p.name.clone()));
                };
                scope.set(&p.name, value);
            }
            Statement::Let(l) => {
                let value = expr::eval(&l.value, &scope)?;
                scope.set(&l.name, value);
            }
            Statement::Block(b) => {
                let mut inputs = Map::new();
                for field in &b.fields {
                    let value = expr::eval(&field.value, &scope)?;
                    inputs.insert(field.name.clone(), value);
                }
                blocks.push(LoadedBlock {
                    name: b.name.clone(),
                    provider: b.provider.clone(),
                    resource: b.resource.clone(),
                    protected: b.protected,
                    inputs,
                });
                // Make block available in scope as a map (outputs filled later by engine)
                scope.set(&b.name, Value::Map(Map::new()));
            }
            Statement::Target(t) => {
                targets.insert(t.name.clone(), t.blocks.clone());
            }
            Statement::Output(o) => {
                let value = expr::eval(&o.value, &scope)?;
                outputs.insert(o.name.clone(), value);
            }
        }
    }

    Ok(LoadedModule {
        blocks,
        targets,
        outputs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    #[test]
    fn load_simple_block() {
        let input = r#"
server = exec {
  command = "go build"
  output = "server"
}
"#;
        let module = parser::parse(input).unwrap();
        let result = load(&module, &Map::new()).unwrap();
        assert_eq!(result.blocks.len(), 1);
        assert_eq!(result.blocks[0].name, "server");
        assert_eq!(result.blocks[0].provider, "exec");
        assert_eq!(
            result.blocks[0].inputs.get("command").unwrap().as_str(),
            Some("go build")
        );
    }

    #[test]
    fn load_let_binding() {
        let input = r#"
let name = "hello"
server = exec {
  command = name
  output = "out"
}
"#;
        let module = parser::parse(input).unwrap();
        let result = load(&module, &Map::new()).unwrap();
        assert_eq!(result.blocks[0].inputs.get("command").unwrap().as_str(), Some("hello"));
    }

    #[test]
    fn load_param_with_value() {
        let input = r#"
param env : string
server = exec {
  command = env
  output = "out"
}
"#;
        let module = parser::parse(input).unwrap();
        let mut params = Map::new();
        params.insert("env".into(), Value::Str("prod".into()));
        let result = load(&module, &params).unwrap();
        assert_eq!(result.blocks[0].inputs.get("command").unwrap().as_str(), Some("prod"));
    }

    #[test]
    fn load_param_with_default() {
        let input = r#"
param replicas : int = 3
server = exec {
  command = "echo"
  output = "out"
}
"#;
        let module = parser::parse(input).unwrap();
        let result = load(&module, &Map::new()).unwrap();
        // Param was set in scope but not used by the block — just verify no error
        assert_eq!(result.blocks.len(), 1);
    }

    #[test]
    fn load_missing_param() {
        let input = "param env : string\n";
        let module = parser::parse(input).unwrap();
        let result = load(&module, &Map::new());
        assert!(matches!(result, Err(LoadError::MissingParam(_))));
    }

    #[test]
    fn load_target() {
        let input = r#"
server = exec {
  command = "build"
  output = "out"
}
target build = [server]
"#;
        let module = parser::parse(input).unwrap();
        let result = load(&module, &Map::new()).unwrap();
        assert_eq!(result.targets["build"], vec!["server"]);
    }

    #[test]
    fn load_output() {
        let input = r#"
let version = "1.0"
output ver = version
"#;
        let module = parser::parse(input).unwrap();
        let result = load(&module, &Map::new()).unwrap();
        assert_eq!(result.outputs.get("ver").unwrap().as_str(), Some("1.0"));
    }

    #[test]
    fn load_interpolation_in_block() {
        let input = r#"
let tag = "latest"
image = exec {
  command = "docker build -t myapp:${tag}"
  output = "image"
}
"#;
        let module = parser::parse(input).unwrap();
        let result = load(&module, &Map::new()).unwrap();
        assert_eq!(
            result.blocks[0].inputs.get("command").unwrap().as_str(),
            Some("docker build -t myapp:latest")
        );
    }

    #[test]
    fn load_multiple_blocks_with_targets() {
        let input = r#"
server = exec {
  command = "go build"
  output = "server"
}
image = exec {
  command = "docker build"
  output = "image"
}
target build = [server, image]
target deploy = [image]
"#;
        let module = parser::parse(input).unwrap();
        let result = load(&module, &Map::new()).unwrap();
        assert_eq!(result.blocks.len(), 2);
        assert_eq!(result.targets.len(), 2);
        assert_eq!(result.targets["build"], vec!["server", "image"]);
        assert_eq!(result.targets["deploy"], vec!["image"]);
    }
}

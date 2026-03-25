use std::collections::HashMap;
use std::error::Error;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::output::BlockWriter;
use crate::value::{Map, Type, Value};

/// Shorthand for the boxed error type used at provider boundaries.
pub type BoxError = Box<dyn Error + Send + Sync>;

/// Result of resolving a block's inputs.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolveResult {
    pub inputs: Vec<ResolvedInput>,
    pub watches: Vec<String>,
    pub platform: Vec<String>,
}

/// A named group of resolved file paths.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedInput {
    pub key: String,
    pub paths: Vec<ResolvedPath>,
}

/// A single file with its content hash.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedPath {
    pub path: String,
    pub content_hash: String,
}

/// What action the plan phase determined is needed.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanAction {
    Create,
    Update,
    Replace,
    Destroy,
    None,
}

/// Result of the plan phase.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanResult {
    pub action: PlanAction,
    pub description: String,
}

/// Result of the apply phase, parameterized by the provider's state type.
#[derive(Debug, Clone)]
pub struct ApplyResult<S> {
    pub outputs: Map,
    pub state: Option<S>,
}

/// Signature of a provider-exported function.
#[derive(Debug, Clone)]
pub struct FuncSignature {
    pub name: String,
    pub params: Vec<FuncParam>,
    pub returns: Type,
}

/// A parameter in a function signature.
#[derive(Debug, Clone)]
pub struct FuncParam {
    pub name: String,
    pub typ: Type,
}

/// Whether a resource produces build artifacts or test results.
#[derive(Debug, Clone, PartialEq)]
pub enum ResourceKind {
    Build,
    Test,
}

/// A provider groups related resources and shared functions.
pub trait Provider {
    fn name(&self) -> &str;
    fn resources(&self) -> Vec<Box<dyn DynResource>>;
    fn functions(&self) -> Vec<FuncSignature>;
    fn call_function(&self, name: &str, args: &[Value]) -> Result<Value, BoxError>;
}

/// Declares a named, typed output that a resource produces.
#[derive(Debug, Clone)]
pub struct OutputSchema {
    pub name: String,
    pub typ: Type,
}

/// A resource with a concrete, type-safe state type.
///
/// Providers implement this. The `DynResource` trait (automatically implemented
/// via blanket impl) handles JSON serialization at the registry boundary.
pub trait Resource {
    type State: Serialize + DeserializeOwned;

    fn name(&self) -> &str;
    fn kind(&self) -> ResourceKind;
    fn outputs(&self) -> Vec<OutputSchema>;
    fn resolve(&self, inputs: &Map) -> Result<ResolveResult, BoxError>;
    fn plan(&self, inputs: &Map, prior_state: Option<&Self::State>) -> Result<PlanResult, BoxError>;
    fn apply(
        &self,
        inputs: &Map,
        prior_state: Option<&Self::State>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<Self::State>, BoxError>;
    fn destroy(&self, prior_state: &Self::State, writer: &BlockWriter) -> Result<(), BoxError>;
    fn refresh(&self, prior_state: &Self::State) -> Result<ApplyResult<Self::State>, BoxError>;
}

/// Object-safe resource trait used by the registry. Serializes state
/// to/from `serde_json::Value` at the boundary.
pub trait DynResource {
    fn name(&self) -> &str;
    fn kind(&self) -> ResourceKind;
    fn outputs(&self) -> Vec<OutputSchema>;
    fn resolve(&self, inputs: &Map) -> Result<ResolveResult, BoxError>;
    fn plan(&self, inputs: &Map, prior_state: Option<&serde_json::Value>) -> Result<PlanResult, BoxError>;
    fn apply(
        &self,
        inputs: &Map,
        prior_state: Option<&serde_json::Value>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<serde_json::Value>, BoxError>;
    fn destroy(&self, prior_state: &serde_json::Value, writer: &BlockWriter) -> Result<(), BoxError>;
    fn refresh(&self, prior_state: &serde_json::Value) -> Result<ApplyResult<serde_json::Value>, BoxError>;
}

/// Blanket impl: any `Resource` automatically becomes a `DynResource`
/// by serializing/deserializing state at the boundary.
impl<R: Resource> DynResource for R {
    fn name(&self) -> &str {
        Resource::name(self)
    }

    fn kind(&self) -> ResourceKind {
        Resource::kind(self)
    }

    fn outputs(&self) -> Vec<OutputSchema> {
        Resource::outputs(self)
    }

    fn resolve(&self, inputs: &Map) -> Result<ResolveResult, BoxError> {
        Resource::resolve(self, inputs)
    }

    fn plan(&self, inputs: &Map, prior_state: Option<&serde_json::Value>) -> Result<PlanResult, BoxError> {
        let state = prior_state.map(|v| serde_json::from_value(v.clone())).transpose()?;
        Resource::plan(self, inputs, state.as_ref())
    }

    fn apply(
        &self,
        inputs: &Map,
        prior_state: Option<&serde_json::Value>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<serde_json::Value>, BoxError> {
        let state = prior_state.map(|v| serde_json::from_value(v.clone())).transpose()?;
        let result = Resource::apply(self, inputs, state.as_ref(), writer)?;
        Ok(ApplyResult {
            outputs: result.outputs,
            state: result.state.map(serde_json::to_value).transpose()?,
        })
    }

    fn destroy(&self, prior_state: &serde_json::Value, writer: &BlockWriter) -> Result<(), BoxError> {
        let state: R::State = serde_json::from_value(prior_state.clone())?;
        Resource::destroy(self, &state, writer)
    }

    fn refresh(&self, prior_state: &serde_json::Value) -> Result<ApplyResult<serde_json::Value>, BoxError> {
        let state: R::State = serde_json::from_value(prior_state.clone())?;
        let result = Resource::refresh(self, &state)?;
        Ok(ApplyResult {
            outputs: result.outputs,
            state: result.state.map(serde_json::to_value).transpose()?,
        })
    }
}

/// Registry for looking up providers by name.
pub struct ProviderRegistry {
    providers: HashMap<String, Box<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    pub fn register(&mut self, provider: Box<dyn Provider>) {
        self.providers.insert(provider.name().to_owned(), provider);
    }

    pub fn get_resource(&self, provider: &str, resource: &str) -> Option<Box<dyn DynResource>> {
        let p = self.providers.get(provider)?;
        p.resources().into_iter().find(|r| r.name() == resource)
    }

    pub fn call_function(&self, provider: &str, name: &str, args: &[Value]) -> Result<Value, BoxError> {
        let p = self
            .providers
            .get(provider)
            .ok_or_else(|| format!("unknown provider: {provider}"))?;
        p.call_function(name, args)
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct StubState {
        version: u32,
    }

    struct StubProvider;

    impl Provider for StubProvider {
        fn name(&self) -> &str {
            "stub"
        }

        fn resources(&self) -> Vec<Box<dyn DynResource>> {
            vec![Box::new(StubResource)]
        }

        fn functions(&self) -> Vec<FuncSignature> {
            vec![]
        }

        fn call_function(&self, name: &str, _args: &[Value]) -> Result<Value, BoxError> {
            Err(format!("unknown function: {name}").into())
        }
    }

    struct StubResource;

    impl Resource for StubResource {
        type State = StubState;

        fn name(&self) -> &str {
            "thing"
        }

        fn kind(&self) -> ResourceKind {
            ResourceKind::Build
        }

        fn outputs(&self) -> Vec<OutputSchema> {
            vec![]
        }

        fn resolve(&self, _inputs: &Map) -> Result<ResolveResult, BoxError> {
            Ok(ResolveResult {
                inputs: vec![],
                watches: vec![],
                platform: vec![],
            })
        }

        fn plan(&self, _inputs: &Map, prior_state: Option<&StubState>) -> Result<PlanResult, BoxError> {
            let action = if prior_state.is_some() {
                PlanAction::Update
            } else {
                PlanAction::Create
            };
            Ok(PlanResult {
                action,
                description: "stub plan".into(),
            })
        }

        fn apply(
            &self,
            _inputs: &Map,
            _prior_state: Option<&StubState>,
            _writer: &BlockWriter,
        ) -> Result<ApplyResult<StubState>, BoxError> {
            Ok(ApplyResult {
                outputs: Map::new(),
                state: Some(StubState { version: 1 }),
            })
        }

        fn destroy(&self, _prior_state: &StubState, _writer: &BlockWriter) -> Result<(), BoxError> {
            Ok(())
        }

        fn refresh(&self, prior_state: &StubState) -> Result<ApplyResult<StubState>, BoxError> {
            Ok(ApplyResult {
                outputs: Map::new(),
                state: Some(prior_state.clone()),
            })
        }
    }

    #[test]
    fn registry_lookup() {
        let mut reg = ProviderRegistry::new();
        reg.register(Box::new(StubProvider));
        assert!(reg.get_resource("stub", "thing").is_some());
        assert!(reg.get_resource("stub", "missing").is_none());
        assert!(reg.get_resource("missing", "thing").is_none());
    }

    #[test]
    fn resource_plan_create() {
        let resource = StubResource;
        let result = Resource::plan(&resource, &Map::new(), None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn resource_plan_update() {
        let resource = StubResource;
        let state = StubState { version: 1 };
        let result = Resource::plan(&resource, &Map::new(), Some(&state)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn apply_returns_typed_state() {
        let resource = StubResource;
        let output = crate::output::Output::new(&[]);
        let writer = output.writer("test");
        let result = Resource::apply(&resource, &Map::new(), None, &writer).unwrap();
        assert_eq!(result.state, Some(StubState { version: 1 }));
    }

    #[test]
    fn dyn_resource_roundtrips_state() {
        let mut reg = ProviderRegistry::new();
        reg.register(Box::new(StubProvider));
        let resource = reg.get_resource("stub", "thing").unwrap();

        // Apply through DynResource — state comes back as JSON
        let output = crate::output::Output::new(&[]);
        let writer = output.writer("test");
        let result = resource.apply(&Map::new(), None, &writer).unwrap();
        let json_state = result.state.unwrap();
        assert_eq!(json_state, serde_json::json!({"version": 1}));

        // Plan with that JSON state — deserialized back to StubState internally
        let plan = resource.plan(&Map::new(), Some(&json_state)).unwrap();
        assert_eq!(plan.action, PlanAction::Update);
    }
}

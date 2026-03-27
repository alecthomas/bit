use std::collections::HashMap;
use std::error::Error;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::output::BlockWriter;
use crate::value::{Map, Value};

/// Shorthand for the boxed error type used at provider boundaries.
pub type BoxError = Box<dyn Error + Send + Sync>;

use std::path::PathBuf;

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

/// Result of the apply phase, with typed state and outputs.
#[derive(Debug, Clone)]
pub struct ApplyResult<S, O> {
    pub outputs: O,
    pub state: Option<S>,
}

/// Signature of a provider-exported function.
#[derive(Debug, Clone)]
pub struct FuncSignature {
    pub name: String,
    pub params: Vec<FuncParam>,
    pub returns: crate::value::Type,
}

/// A parameter in a function signature.
#[derive(Debug, Clone)]
pub struct FuncParam {
    pub name: String,
    pub typ: crate::value::Type,
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

/// A resource with concrete, type-safe state, inputs, and outputs.
///
/// Providers implement this. The `DynResource` trait (automatically implemented
/// via blanket impl) handles serde conversion at the registry boundary.
pub trait Resource {
    type State: Serialize + DeserializeOwned;
    type Inputs: DeserializeOwned;
    type Outputs: Serialize;

    fn name(&self) -> &str;
    fn kind(&self) -> ResourceKind;
    /// Return the list of files that this block depends on.
    fn resolve(&self, inputs: &Self::Inputs) -> Result<Vec<PathBuf>, BoxError>;
    fn plan(&self, inputs: &Self::Inputs, prior_state: Option<&Self::State>) -> Result<PlanResult, BoxError>;
    fn apply(
        &self,
        inputs: &Self::Inputs,
        prior_state: Option<&Self::State>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<Self::State, Self::Outputs>, BoxError>;
    fn destroy(&self, prior_state: &Self::State, writer: &BlockWriter) -> Result<(), BoxError>;
    fn refresh(&self, prior_state: &Self::State) -> Result<ApplyResult<Self::State, Self::Outputs>, BoxError>;
}

/// Object-safe resource trait used by the registry. Converts between
/// `Map` and typed structs via serde at the boundary.
pub trait DynResource {
    fn name(&self) -> &str;
    fn kind(&self) -> ResourceKind;
    fn resolve(&self, inputs: &Map) -> Result<Vec<PathBuf>, BoxError>;
    fn plan(&self, inputs: &Map, prior_state: Option<&serde_json::Value>) -> Result<PlanResult, BoxError>;
    fn apply(
        &self,
        inputs: &Map,
        prior_state: Option<&serde_json::Value>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<serde_json::Value, Map>, BoxError>;
    fn destroy(&self, prior_state: &serde_json::Value, writer: &BlockWriter) -> Result<(), BoxError>;
    fn refresh(&self, prior_state: &serde_json::Value) -> Result<ApplyResult<serde_json::Value, Map>, BoxError>;
}

/// Deserialize a `Map` into a typed struct via serde.
fn deserialize_inputs<T: DeserializeOwned>(map: &Map) -> Result<T, BoxError> {
    let json = serde_json::to_value(map)?;
    Ok(serde_json::from_value(json)?)
}

/// Serialize a typed struct back into a `Map`.
fn serialize_outputs<T: Serialize>(outputs: &T) -> Result<Map, BoxError> {
    let json = serde_json::to_value(outputs)?;
    Ok(serde_json::from_value(json)?)
}

/// Blanket impl: any `Resource` automatically becomes a `DynResource`
/// by serializing/deserializing at the boundary.
impl<R: Resource> DynResource for R {
    fn name(&self) -> &str {
        Resource::name(self)
    }

    fn kind(&self) -> ResourceKind {
        Resource::kind(self)
    }

    fn resolve(&self, inputs: &Map) -> Result<Vec<PathBuf>, BoxError> {
        let typed: R::Inputs = deserialize_inputs(inputs)?;
        Resource::resolve(self, &typed)
    }

    fn plan(&self, inputs: &Map, prior_state: Option<&serde_json::Value>) -> Result<PlanResult, BoxError> {
        let typed: R::Inputs = deserialize_inputs(inputs)?;
        let state = prior_state.map(|v| serde_json::from_value(v.clone())).transpose()?;
        Resource::plan(self, &typed, state.as_ref())
    }

    fn apply(
        &self,
        inputs: &Map,
        prior_state: Option<&serde_json::Value>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<serde_json::Value, Map>, BoxError> {
        let typed: R::Inputs = deserialize_inputs(inputs)?;
        let state = prior_state.map(|v| serde_json::from_value(v.clone())).transpose()?;
        let result = Resource::apply(self, &typed, state.as_ref(), writer)?;
        Ok(ApplyResult {
            outputs: serialize_outputs(&result.outputs)?,
            state: result.state.map(serde_json::to_value).transpose()?,
        })
    }

    fn destroy(&self, prior_state: &serde_json::Value, writer: &BlockWriter) -> Result<(), BoxError> {
        let state: R::State = serde_json::from_value(prior_state.clone())?;
        Resource::destroy(self, &state, writer)
    }

    fn refresh(&self, prior_state: &serde_json::Value) -> Result<ApplyResult<serde_json::Value, Map>, BoxError> {
        let state: R::State = serde_json::from_value(prior_state.clone())?;
        let result = Resource::refresh(self, &state)?;
        Ok(ApplyResult {
            outputs: serialize_outputs(&result.outputs)?,
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

    #[derive(Debug, Deserialize)]
    struct StubInputs {}

    #[derive(Debug, Serialize)]
    struct StubOutputs {}

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
        type Inputs = StubInputs;
        type Outputs = StubOutputs;

        fn name(&self) -> &str {
            "thing"
        }

        fn kind(&self) -> ResourceKind {
            ResourceKind::Build
        }

        fn resolve(&self, _inputs: &StubInputs) -> Result<Vec<PathBuf>, BoxError> {
            Ok(vec![])
        }

        fn plan(&self, _inputs: &StubInputs, prior_state: Option<&StubState>) -> Result<PlanResult, BoxError> {
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
            _inputs: &StubInputs,
            _prior_state: Option<&StubState>,
            _writer: &BlockWriter,
        ) -> Result<ApplyResult<StubState, StubOutputs>, BoxError> {
            Ok(ApplyResult {
                outputs: StubOutputs {},
                state: Some(StubState { version: 1 }),
            })
        }

        fn destroy(&self, _prior_state: &StubState, _writer: &BlockWriter) -> Result<(), BoxError> {
            Ok(())
        }

        fn refresh(&self, prior_state: &StubState) -> Result<ApplyResult<StubState, StubOutputs>, BoxError> {
            Ok(ApplyResult {
                outputs: StubOutputs {},
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
        let result = Resource::plan(&resource, &StubInputs {}, None).unwrap();
        assert_eq!(result.action, PlanAction::Create);
    }

    #[test]
    fn resource_plan_update() {
        let resource = StubResource;
        let state = StubState { version: 1 };
        let result = Resource::plan(&resource, &StubInputs {}, Some(&state)).unwrap();
        assert_eq!(result.action, PlanAction::Update);
    }

    #[test]
    fn apply_returns_typed_state() {
        let resource = StubResource;
        let output = crate::output::Output::new(&[]);
        let writer = output.writer("test");
        let result = Resource::apply(&resource, &StubInputs {}, None, &writer).unwrap();
        assert_eq!(result.state, Some(StubState { version: 1 }));
    }

    #[test]
    fn dyn_resource_roundtrips_state() {
        let mut reg = ProviderRegistry::new();
        reg.register(Box::new(StubProvider));
        let resource = reg.get_resource("stub", "thing").unwrap();

        let output = crate::output::Output::new(&[]);
        let writer = output.writer("test");
        let result = resource.apply(&Map::new(), None, &writer).unwrap();
        let json_state = result.state.unwrap();
        assert_eq!(json_state, serde_json::json!({"version": 1}));

        let plan = resource.plan(&Map::new(), Some(&json_state)).unwrap();
        assert_eq!(plan.action, PlanAction::Update);
    }
}

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::sync::Arc;

use crate::cache::{ArtifactRef, Cas};
use crate::output::BlockWriter;
use crate::schema::Schema;
use crate::sha256::SHA256;
use crate::value::{Map, Value};
pub use crate::value::{StructField, StructType};

/// Shorthand for the boxed error type used at provider boundaries.
pub type BoxError = Box<dyn Error + Send + Sync>;

#[doc(hidden)]
pub fn deserialize_function_value<T: DeserializeOwned>(value: &Value) -> Result<T, BoxError> {
    Ok(serde_json::from_value(serde_json::to_value(value)?)?)
}

#[doc(hidden)]
pub fn serialize_function_value<T: Serialize>(value: &T) -> Result<Value, BoxError> {
    Ok(serde_json::from_value(serde_json::to_value(value)?)?)
}

/// What action the plan phase determined is needed.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanAction {
    Create,
    Update,
    Destroy,
    /// Recreate outputs from a shared cache receipt instead of running the
    /// provider.
    Restore,
    None,
}

/// Whether successful results of a resource may be shared across worktrees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    /// Results stay in worktree-local state (the default).
    Local,
    /// Successful results are published to the shared action cache.
    /// `version` must change whenever the receipt state, output
    /// interpretation, capture, or restoration behaviour changes.
    Shared { version: u32 },
}

/// A provider's classification of a shared receipt for the current context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptCheck {
    /// The receipt's objects already exist and are valid here; skip.
    Valid,
    /// The receipt is usable but its artifacts must be materialized.
    Restore,
    /// The receipt cannot be used in this context; run the provider.
    Unusable,
}

/// Result of the plan phase.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanResult {
    pub action: PlanAction,
    pub description: String,
    /// Optional reason for the action (displayed dimmed).
    pub reason: Option<String>,
}

/// Result of the apply phase, with typed state and outputs.
#[derive(Debug, Clone)]
pub struct ApplyResult<S, O> {
    pub outputs: O,
    pub state: Option<S>,
}

/// Result of materializing a receipt: `None` binds the receipt verbatim.
pub type MaterializeResult<S, O> = Result<Option<ApplyResult<S, O>>, BoxError>;

/// Signature of a provider-exported function.
#[derive(Debug, Clone)]
pub struct FuncSignature {
    pub name: String,
    pub params: Vec<(String, StructField)>,
    pub returns: crate::value::Type,
}

/// Schema describing a resource's interface.
#[derive(Debug, Clone, Serialize)]
pub struct ResourceSchema {
    pub kind: ResourceKind,
    pub inputs: StructType,
    pub outputs: StructType,
}

/// Whether a resource produces build artifacts or test results.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceKind {
    Build,
    Test,
}

/// A provider groups related resources and shared functions.
pub trait Provider: Send + Sync {
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
    type Inputs: DeserializeOwned + Schema;
    type Outputs: Serialize + Schema;

    fn name(&self) -> &str;
    fn kind(&self) -> ResourceKind;
    fn schema(&self) -> ResourceSchema {
        ResourceSchema {
            kind: self.kind(),
            inputs: Self::Inputs::schema(),
            outputs: Self::Outputs::schema(),
        }
    }
    /// Derive additional tracked inputs from the seed inputs and return a map
    /// of key -> SHA256 hash. The engine uses this map for change detection:
    /// if any hash differs from the prior run, the block is re-applied.
    fn resolve(&self, inputs: &Self::Inputs) -> Result<BTreeMap<String, SHA256>, BoxError>;
    fn plan(&self, inputs: &Self::Inputs, prior_state: Option<&Self::State>) -> Result<PlanResult, BoxError>;
    fn apply(
        &self,
        inputs: &Self::Inputs,
        prior_state: Option<&Self::State>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<Self::State, Self::Outputs>, BoxError>;
    fn destroy(&self, prior_state: &Self::State, writer: &BlockWriter) -> Result<(), BoxError>;

    // -- Shared cache contract -------------------------------------------
    //
    // Resources are worktree-local by default. A resource opts into the
    // shared action cache by returning `CachePolicy::Shared`; the engine then
    // owns key computation, receipt storage, and CAS integrity, while the
    // resource owns artifact meaning, destination selection, and contextual
    // output reconstruction.

    fn cache_policy(&self) -> CachePolicy {
        CachePolicy::Local
    }

    /// Keys of `resolve` entries that describe produced outputs rather than
    /// sources. They stay in the local content hash (so a deleted output
    /// triggers a rebuild) but are excluded from the shared action key,
    /// which must be computable before the output exists.
    fn output_keys(&self, _inputs: &Self::Inputs) -> Vec<String> {
        vec![]
    }

    /// Toolchain and environment fingerprint mixed into the action key.
    fn toolchain(&self, _inputs: &Self::Inputs) -> Result<BTreeMap<String, String>, BoxError> {
        Ok(BTreeMap::new())
    }

    /// Capture durable artifacts produced by a successful apply into the CAS,
    /// keyed by a provider-defined role.
    fn capture_artifacts(
        &self,
        _inputs: &Self::Inputs,
        _state: &Self::State,
        _cas: &Cas,
    ) -> Result<BTreeMap<String, ArtifactRef>, BoxError> {
        Ok(BTreeMap::new())
    }

    /// Classify a receipt for the current inputs. The default treats every
    /// receipt as valid, which is correct for validation-only resources
    /// that produce no artifacts.
    fn check_receipt(
        &self,
        _inputs: &Self::Inputs,
        _state: &Self::State,
        _artifacts: &BTreeMap<String, ArtifactRef>,
    ) -> Result<ReceiptCheck, BoxError> {
        Ok(ReceiptCheck::Valid)
    }

    /// Recreate missing objects from the CAS and return state and outputs
    /// resolved for the current worktree. `None` means the receipt's state
    /// and outputs are context-free and can be bound verbatim. A resource
    /// that returns [`ReceiptCheck::Restore`] must return `Some`.
    fn materialize(
        &self,
        _inputs: &Self::Inputs,
        _state: &Self::State,
        _artifacts: &BTreeMap<String, ArtifactRef>,
        _cas: &Cas,
        _writer: &BlockWriter,
    ) -> MaterializeResult<Self::State, Self::Outputs> {
        Ok(None)
    }
}

/// Object-safe resource trait used by the registry. Converts between
/// `Map` and typed structs via serde at the boundary.
pub trait DynResource: Send + Sync {
    fn name(&self) -> &str;
    fn kind(&self) -> ResourceKind;
    fn schema(&self) -> ResourceSchema;
    fn resolve(&self, inputs: &Map) -> Result<BTreeMap<String, SHA256>, BoxError>;
    fn plan(&self, inputs: &Map, prior_state: Option<&serde_json::Value>) -> Result<PlanResult, BoxError>;
    fn apply(
        &self,
        inputs: &Map,
        prior_state: Option<&serde_json::Value>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<serde_json::Value, Map>, BoxError>;
    fn destroy(&self, prior_state: &serde_json::Value, writer: &BlockWriter) -> Result<(), BoxError>;

    fn cache_policy(&self) -> CachePolicy {
        CachePolicy::Local
    }
    fn output_keys(&self, _inputs: &Map) -> Result<Vec<String>, BoxError> {
        Ok(vec![])
    }
    fn toolchain(&self, _inputs: &Map) -> Result<BTreeMap<String, String>, BoxError> {
        Ok(BTreeMap::new())
    }
    fn capture_artifacts(
        &self,
        _inputs: &Map,
        _state: &serde_json::Value,
        _cas: &Cas,
    ) -> Result<BTreeMap<String, ArtifactRef>, BoxError> {
        Ok(BTreeMap::new())
    }
    /// Returns [`ReceiptCheck::Unusable`] when the receipt state cannot be
    /// deserialized for this resource.
    fn check_receipt(
        &self,
        _inputs: &Map,
        _state: &serde_json::Value,
        _artifacts: &BTreeMap<String, ArtifactRef>,
    ) -> Result<ReceiptCheck, BoxError> {
        Ok(ReceiptCheck::Valid)
    }
    fn materialize(
        &self,
        _inputs: &Map,
        _state: &serde_json::Value,
        _artifacts: &BTreeMap<String, ArtifactRef>,
        _cas: &Cas,
        _writer: &BlockWriter,
    ) -> MaterializeResult<serde_json::Value, Map> {
        Ok(None)
    }
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
impl<R: Resource + Send + Sync> DynResource for R {
    fn name(&self) -> &str {
        Resource::name(self)
    }

    fn kind(&self) -> ResourceKind {
        Resource::kind(self)
    }

    fn schema(&self) -> ResourceSchema {
        Resource::schema(self)
    }

    fn resolve(&self, inputs: &Map) -> Result<BTreeMap<String, SHA256>, BoxError> {
        let typed: R::Inputs = deserialize_inputs(inputs)?;
        Resource::resolve(self, &typed)
    }

    fn plan(&self, inputs: &Map, prior_state: Option<&serde_json::Value>) -> Result<PlanResult, BoxError> {
        let typed: R::Inputs = deserialize_inputs(inputs)?;
        // Discard prior state if it can't be deserialized (e.g. resource type changed).
        let state = prior_state.and_then(|v| serde_json::from_value(v.clone()).ok());
        Resource::plan(self, &typed, state.as_ref())
    }

    fn apply(
        &self,
        inputs: &Map,
        prior_state: Option<&serde_json::Value>,
        writer: &BlockWriter,
    ) -> Result<ApplyResult<serde_json::Value, Map>, BoxError> {
        let typed: R::Inputs = deserialize_inputs(inputs)?;
        // Discard prior state if it can't be deserialized (e.g. resource type changed).
        let state = prior_state.and_then(|v| serde_json::from_value(v.clone()).ok());
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

    fn cache_policy(&self) -> CachePolicy {
        Resource::cache_policy(self)
    }

    fn output_keys(&self, inputs: &Map) -> Result<Vec<String>, BoxError> {
        let typed: R::Inputs = deserialize_inputs(inputs)?;
        Ok(Resource::output_keys(self, &typed))
    }

    fn toolchain(&self, inputs: &Map) -> Result<BTreeMap<String, String>, BoxError> {
        let typed: R::Inputs = deserialize_inputs(inputs)?;
        Resource::toolchain(self, &typed)
    }

    fn capture_artifacts(
        &self,
        inputs: &Map,
        state: &serde_json::Value,
        cas: &Cas,
    ) -> Result<BTreeMap<String, ArtifactRef>, BoxError> {
        let typed: R::Inputs = deserialize_inputs(inputs)?;
        let state: R::State = serde_json::from_value(state.clone())?;
        Resource::capture_artifacts(self, &typed, &state, cas)
    }

    fn check_receipt(
        &self,
        inputs: &Map,
        state: &serde_json::Value,
        artifacts: &BTreeMap<String, ArtifactRef>,
    ) -> Result<ReceiptCheck, BoxError> {
        let typed: R::Inputs = deserialize_inputs(inputs)?;
        let Ok(state) = serde_json::from_value::<R::State>(state.clone()) else {
            return Ok(ReceiptCheck::Unusable);
        };
        Resource::check_receipt(self, &typed, &state, artifacts)
    }

    fn materialize(
        &self,
        inputs: &Map,
        state: &serde_json::Value,
        artifacts: &BTreeMap<String, ArtifactRef>,
        cas: &Cas,
        writer: &BlockWriter,
    ) -> MaterializeResult<serde_json::Value, Map> {
        let typed: R::Inputs = deserialize_inputs(inputs)?;
        let state: R::State = serde_json::from_value(state.clone())?;
        let Some(result) = Resource::materialize(self, &typed, &state, artifacts, cas, writer)? else {
            return Ok(None);
        };
        Ok(Some(ApplyResult {
            outputs: serialize_outputs(&result.outputs)?,
            state: result.state.map(serde_json::to_value).transpose()?,
        }))
    }
}

/// Registry for looking up providers by name.
#[derive(Clone)]
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    pub fn register(&mut self, provider: Box<dyn Provider>) {
        self.providers.insert(provider.name().to_owned(), Arc::from(provider));
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

    /// List all registered provider names.
    pub fn provider_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.providers.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    /// List all resources for a provider.
    pub fn provider_resources(&self, provider: &str) -> Vec<Box<dyn DynResource>> {
        self.providers.get(provider).map(|p| p.resources()).unwrap_or_default()
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

    #[derive(Debug, Deserialize, bit_derive::Schema)]
    struct StubInputs {}

    #[derive(Debug, Serialize, bit_derive::Schema)]
    struct StubOutputs {}

    struct StubProvider;

    #[bit_derive::provider_function]
    fn typed_function(value: String, suffix: Option<String>) -> Result<Vec<String>, BoxError> {
        Ok(vec![format!("{value}{}", suffix.unwrap_or_default())])
    }

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

        fn resolve(&self, _inputs: &StubInputs) -> Result<BTreeMap<String, SHA256>, BoxError> {
            Ok(BTreeMap::new())
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
                reason: None,
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
    fn provider_function_macro_generates_signature_and_adapter() {
        let signature = __bit_signature_typed_function();
        assert_eq!(signature.name, "typed_function");
        assert_eq!(signature.params[0].1.typ, crate::value::Type::String);
        assert_eq!(
            signature.params[1].1.typ,
            crate::value::Type::Optional(Box::new(crate::value::Type::String))
        );
        assert_eq!(
            signature.returns,
            crate::value::Type::List(Box::new(crate::value::Type::String))
        );

        assert_eq!(
            __bit_call_typed_function(&[Value::Str("value".into()), Value::Str("-suffix".into())]).unwrap(),
            Value::List(crate::value::Type::String, vec![Value::Str("value-suffix".into())])
        );
        assert_eq!(
            __bit_call_typed_function(&[Value::Str("value".into())]).unwrap(),
            Value::List(crate::value::Type::String, vec![Value::Str("value".into())])
        );
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

    #[test]
    fn schema_json_serialization() {
        use crate::value::{StructField, StructType, Type};

        let schema = ResourceSchema {
            kind: ResourceKind::Build,
            inputs: StructType {
                description: Some("Test resource".into()),
                fields: vec![
                    (
                        "name".into(),
                        StructField {
                            typ: Type::String,
                            default: None,
                            description: Some("The name".into()),
                        },
                    ),
                    (
                        "count".into(),
                        StructField {
                            typ: Type::Optional(Box::new(Type::Number)),
                            default: Some(Value::Number(42.into())),
                            description: None,
                        },
                    ),
                ],
            },
            outputs: StructType {
                description: None,
                fields: vec![(
                    "path".into(),
                    StructField {
                        typ: Type::String,
                        default: None,
                        description: Some("Output path".into()),
                    },
                )],
            },
        };

        let json = serde_json::to_value(&schema).unwrap();
        assert_eq!(json["kind"], "build");
        assert_eq!(json["inputs"]["description"], "Test resource");
        assert_eq!(json["inputs"]["fields"][0]["name"], "name");
        assert_eq!(json["inputs"]["fields"][0]["type"], "string");
        assert_eq!(json["inputs"]["fields"][0]["description"], "The name");
        assert_eq!(json["inputs"]["fields"][1]["name"], "count");
        assert_eq!(json["inputs"]["fields"][1]["type"], "number?");
        assert_eq!(json["inputs"]["fields"][1]["default"], "42");
        assert_eq!(json["outputs"]["fields"][0]["name"], "path");
        assert_eq!(json["outputs"]["fields"][0]["type"], "string");
    }
}

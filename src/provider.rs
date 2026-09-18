use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::path::Path;
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
pub fn serialize_function_value<T: Serialize>(value: &T, typ: &crate::value::Type) -> Result<Value, BoxError> {
    let mut value: Value = serde_json::from_value(serde_json::to_value(value)?)?;
    if let (Value::List(actual, _), crate::value::Type::List(expected)) = (&mut value, typ) {
        *actual = expected.as_ref().clone();
    }
    Ok(value)
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
    /// Results stay in worktree-local state.
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

/// One file a resource declared as an output, in the two spellings the cache
/// needs.
///
/// `role` is the project-relative path the engine derived, and is what a
/// receipt records: it is the same string in every worktree of a repository,
/// so a receipt written by one is legible to the others. `path` is where that
/// file lives for this process, which is what any filesystem access must use.
/// The two differ whenever a block names an output by absolute path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputFile {
    pub role: String,
    pub path: String,
}

/// Store the declared output files in the CAS under their roles. This is the
/// default [`Resource::capture_artifacts`], exposed for resources that capture
/// these files alongside artifacts of their own.
pub fn capture_output_files(outputs: &[OutputFile], cas: &Cas) -> Result<BTreeMap<String, ArtifactRef>, BoxError> {
    let mut artifacts = BTreeMap::new();
    for output in outputs {
        let path = Path::new(&output.path);
        let artifact = if path.is_dir() {
            cas.put_tree(path)?
        } else if path.is_file() {
            cas.put_file(path)?
        } else {
            return Err(format!("output `{}` does not exist", output.role).into());
        };
        artifacts.insert(output.role.clone(), artifact);
    }
    Ok(artifacts)
}

/// Classify a receipt by comparing its captured outputs against the files they
/// describe in this worktree. This is the default [`Resource::check_receipt`].
pub fn check_output_files(
    outputs: &[OutputFile],
    artifacts: &BTreeMap<String, ArtifactRef>,
    cas: &Cas,
) -> ReceiptCheck {
    let mut restore = false;
    for output in outputs {
        let Some(artifact) = artifacts.get(&output.role) else {
            return ReceiptCheck::Unusable;
        };
        if !cas.matches(artifact, Path::new(&output.path)) {
            restore = true;
        }
    }
    if restore {
        ReceiptCheck::Restore
    } else {
        ReceiptCheck::Valid
    }
}

/// Recreate the declared output files from the CAS, leaving alone any that
/// already hold the captured content.
///
/// A resource whose state or outputs embed worktree-specific values calls this
/// from [`Resource::materialize`] and then rebuilds those values from its own
/// inputs, so that nothing from the producing worktree leaks into this one.
pub fn restore_output_files(
    outputs: &[OutputFile],
    artifacts: &BTreeMap<String, ArtifactRef>,
    cas: &Cas,
    writer: &BlockWriter,
) -> Result<(), BoxError> {
    for output in outputs {
        let artifact = artifacts
            .get(&output.role)
            .ok_or_else(|| format!("receipt has no artifact for output `{}`", output.role))?;
        let path = Path::new(&output.path);
        if cas.matches(artifact, path) {
            continue;
        }
        writer.line(&format!("restore {}", output.path));
        cas.materialize(artifact, path)?;
    }
    Ok(())
}

/// Signature of a provider-exported function.
#[derive(Debug, Clone)]
pub struct FuncSignature {
    pub name: String,
    pub description: Option<String>,
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
    // A resource opts into the shared action cache by returning
    // `CachePolicy::Shared`; the engine then owns key computation, receipt
    // storage, and CAS integrity, while the resource owns artifact meaning,
    // destination selection, and contextual output reconstruction.

    /// Whether successful results may be shared across worktrees. Every
    /// resource states this explicitly: whether a result is safe to share
    /// depends on whether the action key captures everything it depends on,
    /// which only the resource knows.
    ///
    /// The answer may depend on the inputs. A resource that usually produces
    /// files but can also be pointed at external state returns `Local` for
    /// the latter, rather than publishing receipts nothing can use.
    fn cache_policy(&self, inputs: &Self::Inputs) -> CachePolicy;

    /// Keys of `resolve` entries that describe produced outputs rather than
    /// sources. They stay in the local content hash (so a deleted output
    /// triggers a rebuild) but are excluded from the shared action key,
    /// which must be computable before the output exists.
    ///
    /// The engine also pairs each key with its project-relative form and
    /// hands the result to the cache methods below as [`OutputFile`]s, so
    /// these must be paths this process can read and write.
    ///
    /// A shared resource must name every file its consumers depend on. An
    /// omitted output is not a missed optimisation: the receipt is published
    /// as if complete, so another worktree restores a partial result and the
    /// block reports success over it.
    fn output_keys(&self, _inputs: &Self::Inputs) -> Vec<String> {
        vec![]
    }

    /// Toolchain and environment fingerprint mixed into the action key.
    fn toolchain(&self, _inputs: &Self::Inputs) -> Result<BTreeMap<String, String>, BoxError> {
        Ok(BTreeMap::new())
    }

    /// Capture durable artifacts produced by a successful apply into the CAS,
    /// keyed by a provider-defined role.
    ///
    /// The default stores the files named by [`Resource::output_keys`], which
    /// covers any resource whose outputs are ordinary files. Override it for
    /// artifacts that do not live on disk, such as a container image.
    fn capture_artifacts(
        &self,
        _inputs: &Self::Inputs,
        _state: &Self::State,
        outputs: &[OutputFile],
        cas: &Cas,
    ) -> Result<BTreeMap<String, ArtifactRef>, BoxError> {
        capture_output_files(outputs, cas)
    }

    /// Classify a receipt for the current inputs. The default compares the
    /// captured outputs against the destinations [`Resource::output_keys`]
    /// names here, and reports [`ReceiptCheck::Valid`] when a resource
    /// declares none, which is correct for validation-only resources.
    fn check_receipt(
        &self,
        _inputs: &Self::Inputs,
        _state: &Self::State,
        outputs: &[OutputFile],
        artifacts: &BTreeMap<String, ArtifactRef>,
        cas: &Cas,
    ) -> Result<ReceiptCheck, BoxError> {
        Ok(check_output_files(outputs, artifacts, cas))
    }

    /// Recreate missing objects from the CAS and return state and outputs
    /// resolved for the current worktree. `None` means the receipt's own
    /// state and outputs are context-free and can be bound as they are.
    ///
    /// The default recreates the declared output files and returns `None`,
    /// which is right for a resource whose state and outputs are derived from
    /// its inputs. Override it when either embeds something specific to the
    /// worktree that produced it, and call [`restore_output_files`] from the
    /// override to recreate the files.
    fn materialize(
        &self,
        _inputs: &Self::Inputs,
        _state: &Self::State,
        outputs: &[OutputFile],
        artifacts: &BTreeMap<String, ArtifactRef>,
        cas: &Cas,
        writer: &BlockWriter,
    ) -> MaterializeResult<Self::State, Self::Outputs> {
        restore_output_files(outputs, artifacts, cas, writer)?;
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

    /// Returns [`CachePolicy::Local`] when the inputs cannot be deserialized
    /// for this resource, so an unusable block is never published.
    fn cache_policy(&self, inputs: &Map) -> CachePolicy;

    // The remaining defaults describe a resource with no cached outputs.
    // Anything implementing `Resource` reaches the richer defaults there
    // through the blanket impl below instead.
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
        _outputs: &[OutputFile],
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
        _outputs: &[OutputFile],
        _artifacts: &BTreeMap<String, ArtifactRef>,
        _cas: &Cas,
    ) -> Result<ReceiptCheck, BoxError> {
        Ok(ReceiptCheck::Valid)
    }
    fn materialize(
        &self,
        _inputs: &Map,
        _state: &serde_json::Value,
        _outputs: &[OutputFile],
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

    fn cache_policy(&self, inputs: &Map) -> CachePolicy {
        match deserialize_inputs::<R::Inputs>(inputs) {
            Ok(typed) => Resource::cache_policy(self, &typed),
            Err(_) => CachePolicy::Local,
        }
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
        outputs: &[OutputFile],
        cas: &Cas,
    ) -> Result<BTreeMap<String, ArtifactRef>, BoxError> {
        let typed: R::Inputs = deserialize_inputs(inputs)?;
        let state: R::State = serde_json::from_value(state.clone())?;
        Resource::capture_artifacts(self, &typed, &state, outputs, cas)
    }

    fn check_receipt(
        &self,
        inputs: &Map,
        state: &serde_json::Value,
        outputs: &[OutputFile],
        artifacts: &BTreeMap<String, ArtifactRef>,
        cas: &Cas,
    ) -> Result<ReceiptCheck, BoxError> {
        let typed: R::Inputs = deserialize_inputs(inputs)?;
        let Ok(state) = serde_json::from_value::<R::State>(state.clone()) else {
            return Ok(ReceiptCheck::Unusable);
        };
        Resource::check_receipt(self, &typed, &state, outputs, artifacts, cas)
    }

    fn materialize(
        &self,
        inputs: &Map,
        state: &serde_json::Value,
        outputs: &[OutputFile],
        artifacts: &BTreeMap<String, ArtifactRef>,
        cas: &Cas,
        writer: &BlockWriter,
    ) -> MaterializeResult<serde_json::Value, Map> {
        let typed: R::Inputs = deserialize_inputs(inputs)?;
        let state: R::State = serde_json::from_value(state.clone())?;
        let Some(result) = Resource::materialize(self, &typed, &state, outputs, artifacts, cas, writer)? else {
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

    /// List all functions for a provider.
    pub fn provider_functions(&self, provider: &str) -> Vec<FuncSignature> {
        self.providers.get(provider).map(|p| p.functions()).unwrap_or_default()
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

    /// Return a value with an optional suffix.
    #[bit_derive::provider_function]
    fn typed_function(value: String, suffix: Option<String>) -> Result<Vec<String>, BoxError> {
        Ok(vec![format!("{value}{}", suffix.unwrap_or_default())])
    }

    #[bit_derive::provider_function]
    fn typed_block_refs(empty: bool) -> Result<Vec<crate::value::BlockRef>, BoxError> {
        Ok(if empty {
            Vec::new()
        } else {
            vec![crate::value::BlockRef::new("build[core]")]
        })
    }

    impl Provider for StubProvider {
        fn name(&self) -> &str {
            "stub"
        }

        fn resources(&self) -> Vec<Box<dyn DynResource>> {
            vec![Box::new(StubResource)]
        }

        fn functions(&self) -> Vec<FuncSignature> {
            vec![__bit_signature_typed_function()]
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

        fn cache_policy(&self, _inputs: &StubInputs) -> CachePolicy {
            CachePolicy::Local
        }
    }

    #[test]
    fn registry_lookup() {
        let mut reg = ProviderRegistry::new();
        reg.register(Box::new(StubProvider));
        assert!(reg.get_resource("stub", "thing").is_some());
        assert!(reg.get_resource("stub", "missing").is_none());
        assert!(reg.get_resource("missing", "thing").is_none());
        assert_eq!(reg.provider_functions("stub")[0].name, "typed_function");
        assert!(reg.provider_functions("missing").is_empty());
    }

    #[test]
    fn provider_function_macro_generates_signature_and_adapter() {
        let signature = __bit_signature_typed_function();
        assert_eq!(signature.name, "typed_function");
        assert_eq!(
            signature.description.as_deref(),
            Some("Return a value with an optional suffix.")
        );
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
    fn provider_function_macro_preserves_block_reference_type() {
        let signature = __bit_signature_typed_block_refs();
        assert_eq!(
            signature.returns,
            crate::value::Type::List(Box::new(crate::value::Type::BlockRef))
        );
        assert_eq!(
            __bit_call_typed_block_refs(&[Value::Bool(false)]).unwrap(),
            Value::List(
                crate::value::Type::BlockRef,
                vec![Value::BlockRef("build[core]".into())]
            )
        );
        assert_eq!(
            __bit_call_typed_block_refs(&[Value::Bool(true)]).unwrap(),
            Value::List(crate::value::Type::BlockRef, Vec::new())
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

    /// Create `dir/name` as a declared output: recorded under its bare name,
    /// living at an absolute path, as a block naming an absolute output would
    /// produce.
    fn write_output(dir: &std::path::Path, name: &str, content: &str) -> OutputFile {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        OutputFile {
            role: name.to_owned(),
            path: path.to_string_lossy().into_owned(),
        }
    }

    #[test]
    fn captures_declared_outputs_under_their_roles() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::new(dir.path().join("cas"));
        let first = write_output(dir.path(), "a.txt", "one");
        let second = write_output(dir.path(), "b.txt", "two");

        let artifacts = capture_output_files(&[first, second], &cas).unwrap();

        assert_eq!(artifacts.keys().collect::<Vec<_>>(), ["a.txt", "b.txt"]);
        assert!(cas.contains(&artifacts["a.txt"].file().unwrap().digest));
        assert!(cas.contains(&artifacts["b.txt"].file().unwrap().digest));
    }

    /// The point of separating role from path: a receipt written where the
    /// output sat at one absolute path is readable where it sits at another.
    #[test]
    fn a_receipt_is_readable_where_the_output_has_a_different_path() {
        let producer = tempfile::tempdir().unwrap();
        let consumer = tempfile::tempdir().unwrap();
        let cas = Cas::new(producer.path().join("cas"));
        let artifacts = capture_output_files(&[write_output(producer.path(), "a.txt", "one")], &cas).unwrap();

        let here = OutputFile {
            role: "a.txt".to_owned(),
            path: consumer.path().join("a.txt").to_string_lossy().into_owned(),
        };
        assert_eq!(
            check_output_files(std::slice::from_ref(&here), &artifacts, &cas),
            ReceiptCheck::Restore
        );

        let output = crate::output::Output::new(&[]);
        restore_output_files(std::slice::from_ref(&here), &artifacts, &cas, &output.writer("block")).unwrap();

        assert_eq!(std::fs::read_to_string(&here.path).unwrap(), "one");
        assert_eq!(check_output_files(&[here], &artifacts, &cas), ReceiptCheck::Valid);
    }

    #[test]
    fn capture_rejects_a_missing_output() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::new(dir.path().join("cas"));
        let missing = OutputFile {
            role: "gone.txt".to_owned(),
            path: dir.path().join("gone.txt").to_string_lossy().into_owned(),
        };

        let err = capture_output_files(&[missing], &cas).unwrap_err().to_string();

        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn captures_and_restores_a_directory_output() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::new(dir.path().join("cas"));
        let tree = dir.path().join("dist");
        std::fs::create_dir_all(tree.join("assets")).unwrap();
        std::fs::write(tree.join("index.js"), "main").unwrap();
        std::fs::write(tree.join("assets/app.css"), "body{}").unwrap();
        let output = OutputFile {
            role: "dist".to_owned(),
            path: tree.to_string_lossy().into_owned(),
        };

        let artifacts = capture_output_files(std::slice::from_ref(&output), &cas).unwrap();
        assert_eq!(
            check_output_files(std::slice::from_ref(&output), &artifacts, &cas),
            ReceiptCheck::Valid
        );

        std::fs::remove_dir_all(&tree).unwrap();
        let writer = crate::output::Output::new(&[]);
        restore_output_files(std::slice::from_ref(&output), &artifacts, &cas, &writer.writer("block")).unwrap();

        assert_eq!(std::fs::read_to_string(tree.join("index.js")).unwrap(), "main");
        assert_eq!(std::fs::read_to_string(tree.join("assets/app.css")).unwrap(), "body{}");
        assert_eq!(check_output_files(&[output], &artifacts, &cas), ReceiptCheck::Valid);
    }

    #[test]
    fn check_reports_valid_when_a_resource_declares_no_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::new(dir.path().join("cas"));
        assert_eq!(check_output_files(&[], &BTreeMap::new(), &cas), ReceiptCheck::Valid);
    }

    #[test]
    fn check_reports_valid_when_outputs_already_match() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::new(dir.path().join("cas"));
        let output = write_output(dir.path(), "a.txt", "one");
        let artifacts = capture_output_files(std::slice::from_ref(&output), &cas).unwrap();

        assert_eq!(check_output_files(&[output], &artifacts, &cas), ReceiptCheck::Valid);
    }

    #[test]
    fn check_reports_restore_when_an_output_is_absent_or_stale() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::new(dir.path().join("cas"));
        let output = write_output(dir.path(), "a.txt", "one");
        let artifacts = capture_output_files(std::slice::from_ref(&output), &cas).unwrap();

        std::fs::write(&output.path, "changed").unwrap();
        assert_eq!(
            check_output_files(std::slice::from_ref(&output), &artifacts, &cas),
            ReceiptCheck::Restore
        );

        std::fs::remove_file(&output.path).unwrap();
        assert_eq!(check_output_files(&[output], &artifacts, &cas), ReceiptCheck::Restore);
    }

    #[test]
    fn check_reports_unusable_when_the_receipt_lacks_an_output() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::new(dir.path().join("cas"));
        let first = write_output(dir.path(), "a.txt", "one");
        let second = write_output(dir.path(), "b.txt", "two");
        let artifacts = capture_output_files(std::slice::from_ref(&first), &cas).unwrap();

        assert_eq!(
            check_output_files(&[first, second], &artifacts, &cas),
            ReceiptCheck::Unusable
        );
    }

    #[test]
    fn restores_a_deleted_output_with_its_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::new(dir.path().join("cas"));
        let output = write_output(dir.path(), "a.txt", "one");
        std::fs::set_permissions(&output.path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let artifacts = capture_output_files(std::slice::from_ref(&output), &cas).unwrap();
        std::fs::remove_file(&output.path).unwrap();

        let writer = crate::output::Output::new(&[]);
        restore_output_files(std::slice::from_ref(&output), &artifacts, &cas, &writer.writer("block")).unwrap();

        assert_eq!(std::fs::read_to_string(&output.path).unwrap(), "one");
        let mode = std::fs::metadata(&output.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
    }

    #[test]
    fn restore_fails_when_the_receipt_lacks_an_output() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Cas::new(dir.path().join("cas"));
        let output = write_output(dir.path(), "a.txt", "one");
        let writer = crate::output::Output::new(&[]);

        let err = restore_output_files(&[output], &BTreeMap::new(), &cas, &writer.writer("block"))
            .unwrap_err()
            .to_string();

        assert!(err.contains("receipt has no artifact for output"), "{err}");
    }
}

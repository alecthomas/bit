use crate::value::StructType;

/// Trait for types that can describe their schema.
/// Derived automatically via `#[derive(Schema)]` from `bit-derive`.
pub trait Schema {
    fn schema() -> StructType;
}

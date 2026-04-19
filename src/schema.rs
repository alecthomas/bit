use crate::value::{StructType, Type};

/// Trait for structs that describe their schema as a `StructType`.
/// Derived automatically via `#[derive(Schema)]` from `bit-derive`.
pub trait Schema {
    fn schema() -> StructType;
}

/// Trait for any type — struct, enum, scalar — that can describe itself
/// as a `Type`. The `Schema` derive macro also implements this, returning
/// `Type::Struct(Self::schema())`. Enums with `#[serde(untagged)]`
/// variants should implement this manually, returning `Type::Union(...)`.
pub trait SchemaType {
    fn schema_type() -> Type;
}

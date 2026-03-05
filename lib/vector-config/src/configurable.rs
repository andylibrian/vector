//! A type that can be represented in a Vector configuration.
//!
//! In Vector, we want to be able to generate a schema for our configuration such that we can have a Rust-agnostic
//! definition of exactly what is configurable, what values are allowed, what bounds exist, and so on and so forth.
//!
//! `Configurable` provides the machinery to allow describing and encoding the shape of a type, recursively, so that by
//! instrumenting all transitive types of the configuration, the schema can be discovered by generating the schema from
//! some root type.
//!
//! # Schema Generation
//!
//! The primary use case for `Configurable` is generating JSON Schema for:
//! - Configuration validation (catch invalid configs before runtime)
//! - Documentation generation (auto-generate component docs for vector.dev)
//! - IDE integration (autocomplete, type checking in editors)
//! - API schema exposure (GraphQL API knows all configurable options)
//!
//! # Implementing Configurable
//!
//! Most types derive `Configurable` automatically:
//!
//! ```ignore
//! #[derive(Configurable)]
//! struct MyConfig {
//!     /// This field's description becomes part of the schema
//!     #[configurable(description = "The number of items to process")]
//!     count: usize,
//! }
//! ```
//!
//! For complex types (external types, types with generics, etc.),
//! manual implementations are needed:
//!
//! ```ignore
//! impl Configurable for String {
//!     fn referenceable_name() -> Option<&'static str> {
//!         Some("std::string")  // Unique identifier for this type
//!     }
//!     
//!     fn generate_schema(_: &RefCell<SchemaGenerator>) -> Result<SchemaObject, GenerateError> {
//!         // Generate JSON Schema for a string type
//!         Ok(SchemaObject::string())
//!     }
//! }
//! ```
//!
//! # Referenceable Names
//!
//! When a type has a `referenceable_name()`, it becomes a reusable schema definition:
//!
//! ```json
//! {
//!   "$ref": "#/definitions/std::string",
//!   "type": "string"
//! }
//! ```
//!
//! Instead of inlining the schema everywhere the type is used, we define
//! it once and reference it by name. This:
//! - Reduces schema size (avoid duplication)
//! - Enables schema evolution (update in one place, affects all uses)
//! - Makes schemas more readable (named sections vs inline definitions)
//!
//! # Metadata
//!
//! Every configurable type has associated metadata:
//!
//! ```ignore
//! let mut metadata = Metadata::default();
//! metadata.set_description("The number of items to process");
//! metadata.add_custom_attribute(CustomAttribute::kv("docs::examples", 10));
//! ```
//!
//! Metadata captures:
//! - Description (becomes schema description)
//! - Title (becomes schema title)
//! - Custom attributes (used for documentation, validation, etc.)
//! - Default values
//! - Validation rules
//!
//! # The ConfigurableRef Pattern
//!
//! Sometimes we need to pass around a "reference" to a configurable type without
//! having the actual type. This is what `ConfigurableRef` provides:
//!
//! ```ignore
//! pub struct ConfigurableRef {
//!     type_name: fn() -> &'static str,
//!     referenceable_name: fn() -> Option<&'static str>,
//!     make_metadata: fn() -> Metadata,
//!     validate_metadata: fn(&Metadata) -> Result<(), GenerateError>,
//!     generate_schema: fn(&RefCell<SchemaGenerator>) -> Result<SchemaObject, GenerateError>,
//! }
//! ```
//!
//! This is essentially a vtable of function pointers. It allows:
//! - Storing heterogeneous types in collections
//! - Late binding (schema generation on demand)
//! - Avoiding monomorphization (no generic explosion)
//!
//! **Why not just use trait objects?**
//!
//! Trait objects (`Box<dyn Trait>`) require the type to be behind a pointer.
//! `ConfigurableRef` is a small, stack-allocated struct that just holds function pointers.
//! This is more efficient when you only need to call trait methods (not store the whole type).

#![deny(missing_docs)]

use std::cell::RefCell;

use serde_json::Value;

use crate::{
    GenerateError, Metadata,
    schema::{SchemaGenerator, SchemaObject},
};

/// A type that can be represented in a Vector configuration.
///
/// In Vector, we want to be able to generate a schema for our configuration such that we can have a Rust-agnostic
/// definition of exactly what is configurable, what values are allowed, what bounds exist, and so on and so forth.
///
/// `Configurable` provides the machinery to allow describing and encoding the shape of a type, recursively, so that by
/// instrumenting all transitive types of the configuration, the schema can be discovered by generating the schema from
/// some root type.
pub trait Configurable {
    /// Gets the referenceable name of this value, if any.
    ///
    /// When specified, this implies the value is both complex and standardized, and should be
    /// reused within any generated schema it is present in.
    fn referenceable_name() -> Option<&'static str>
    where
        Self: Sized,
    {
        None
    }

    /// Whether or not this value is optional.
    ///
    /// This is specifically used to determine when a field is inherently optional, such as a field
    /// that is a true map like `HashMap<K, V>`. This doesn't apply to objects (i.e. structs)
    /// because structs are implicitly non-optional: they have a fixed shape and size, and so on.
    ///
    /// Maps, by definition, are inherently free-form, and thus inherently optional. Thus, this
    /// method should likely not be overridden except for implementing `Configurable` for map
    /// types. If you're using it for something else, you are expected to know what you're doing.
    fn is_optional() -> bool
    where
        Self: Sized,
    {
        false
    }

    /// Gets the metadata for this value.
    fn metadata() -> Metadata
    where
        Self: Sized,
    {
        Metadata::default()
    }

    /// Validates the given metadata against this type.
    ///
    /// This allows for validating specific aspects of the given metadata, such as validation
    /// bounds, and so on, to ensure they are valid for the given type. In some cases, such as with
    /// numeric types, there is a limited amount of validation that can occur within the
    /// `Configurable` derive macro, and additional validation must happen at runtime when the
    /// `Configurable` trait is being used, which this method allows for.
    fn validate_metadata(_metadata: &Metadata) -> Result<(), GenerateError>
    where
        Self: Sized,
    {
        Ok(())
    }

    /// Generates the schema for this value.
    ///
    /// # Errors
    ///
    /// If an error occurs while generating the schema, an error variant will be returned describing
    /// the issue.
    fn generate_schema(generator: &RefCell<SchemaGenerator>) -> Result<SchemaObject, GenerateError>
    where
        Self: Sized;

    /// Create a new configurable reference table.
    fn as_configurable_ref() -> ConfigurableRef
    where
        Self: Sized + 'static,
    {
        ConfigurableRef::new::<Self>()
    }
}

/// A type that can be converted directly to a `serde_json::Value`. This is used when translating
/// the default value in a `Metadata` into a schema object.
pub trait ToValue {
    /// Convert this value into a `serde_json::Value`. Must not fail.
    fn to_value(&self) -> Value;
}

/// A pseudo-reference to a type that can be represented in a Vector configuration. This is
/// composed of references to all the class trait functions.
pub struct ConfigurableRef {
    // TODO: Turn this into a plain value once this is resolved:
    // https://github.com/rust-lang/rust/issues/63084
    type_name: fn() -> &'static str,
    // TODO: Turn this into a plain value once const trait functions are implemented
    // Ref: https://github.com/rust-lang/rfcs/pull/911
    referenceable_name: fn() -> Option<&'static str>,
    make_metadata: fn() -> Metadata,
    validate_metadata: fn(&Metadata) -> Result<(), GenerateError>,
    generate_schema: fn(&RefCell<SchemaGenerator>) -> Result<SchemaObject, GenerateError>,
}

impl ConfigurableRef {
    /// Create a new configurable reference table.
    pub const fn new<T: Configurable + 'static>() -> Self {
        Self {
            type_name: std::any::type_name::<T>,
            referenceable_name: T::referenceable_name,
            make_metadata: T::metadata,
            validate_metadata: T::validate_metadata,
            generate_schema: T::generate_schema,
        }
    }

    pub(crate) fn type_name(&self) -> &'static str {
        (self.type_name)()
    }
    pub(crate) fn referenceable_name(&self) -> Option<&'static str> {
        (self.referenceable_name)()
    }
    pub(crate) fn make_metadata(&self) -> Metadata {
        (self.make_metadata)()
    }
    pub(crate) fn validate_metadata(&self, metadata: &Metadata) -> Result<(), GenerateError> {
        (self.validate_metadata)(metadata)
    }
    pub(crate) fn generate_schema(
        &self,
        generator: &RefCell<SchemaGenerator>,
    ) -> Result<SchemaObject, GenerateError> {
        (self.generate_schema)(generator)
    }
}

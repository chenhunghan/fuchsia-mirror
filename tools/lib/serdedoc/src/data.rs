// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Result, anyhow, bail};
use schemars::Schema;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The data that describes the entire serde user interface.
/// This is extracted from a json schema.
#[derive(Debug, Serialize, PartialEq)]
pub struct AllData {
    /// The base url path to append to all links.
    pub url_path: String,
    /// The root data type.
    pub root: String,
    /// All data types, including the root.
    /// This contains the data for each struct/enum/etc.
    pub data_types: BTreeMap<String, DataType>,
}

impl AllData {
    /// Construct AllData from a root json schema.
    pub fn from_root_schema(url_path: &String, root_schema: &Schema) -> Result<Self> {
        let url_path = url_path.clone();
        let mut data_types = BTreeMap::new();
        let root_type = DataType::from_root_schema(root_schema)?;
        data_types.insert(root_type.rust_type.clone(), root_type.clone());
        let defs = root_schema
            .get("$defs")
            .or_else(|| root_schema.get("definitions"))
            .and_then(|v| v.as_object());
        if let Some(defs) = defs {
            for (rust_type, schema_val) in defs {
                let schema: &Schema = schema_val.try_into()?;
                let child = DataType::from_schema(rust_type.clone(), schema)?;
                data_types.insert(child.rust_type.clone(), child);
            }
        }
        Ok(Self { url_path, root: root_type.rust_type, data_types })
    }
}

/// Data for a single struct/enum/etc.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct DataType {
    /// The rust type.
    pub rust_type: String,
    /// The rust doc-comment.
    pub description: String,
    #[serde(flatten)]
    pub inner: DataTypeInner,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(untagged)]
pub enum DataTypeInner {
    Primitive(PrimitiveDataType),
    Enum(EnumDataType),
    Struct(StructDataType),
}

/// A primitive data type, such as a u8 or String.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct PrimitiveDataType {
    /// The data type name.
    pub data_type: String,
}

/// An enum data type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct EnumDataType {
    /// The variants of the enum.
    /// Note that this does not currently support complex enums or descriptions.
    /// The schemars crate does not populate descriptions for enums.
    pub variants: BTreeSet<String>,
}

/// A struct data type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct StructDataType {
    /// The fields of the struct.
    pub fields: BTreeSet<StructFieldData>,
}

/// A single struct field, which should point to a sub-data-type.
#[derive(Debug, Clone, Eq, Serialize)]
pub struct StructFieldData {
    /// The name of the field.
    pub field_name: String,
    /// The doc-comment for the field.
    pub description: String,
    /// The default value of the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// The data type of the field.
    #[serde(flatten)]
    pub data_type: StructFieldType,
}

/// The type of a single struct field.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum StructFieldType {
    Primitive { data_type: String },
    Custom { data_type: String },
}

impl PartialEq for StructFieldData {
    fn eq(&self, other: &Self) -> bool {
        self.field_name == other.field_name
    }
}

impl Ord for StructFieldData {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.field_name.cmp(&self.field_name)
    }
}

impl PartialOrd for StructFieldData {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(&other))
    }
}

fn strip_ref_prefix(reference: &str) -> String {
    reference
        .strip_prefix("#/$defs/")
        .or_else(|| reference.strip_prefix("#/definitions/"))
        .or_else(|| reference.strip_prefix("#/"))
        .unwrap_or(reference)
        .to_string()
}

fn extract_unit_variants_from_one_of(one_of: &[Value]) -> Option<BTreeSet<String>> {
    let mut variants = BTreeSet::new();
    for item in one_of {
        let obj = item.as_object()?;
        if obj.get("type").and_then(|v| v.as_str()) != Some("string") {
            return None;
        }
        if let Some(const_val) = obj.get("const") {
            variants.insert(const_val.to_string());
        } else if let Some(enum_vals) = obj.get("enum").and_then(|v| v.as_array()) {
            for v in enum_vals {
                variants.insert(v.to_string());
            }
        } else {
            return None;
        }
    }
    if variants.is_empty() { None } else { Some(variants) }
}

impl DataType {
    /// Construct a DataType from a root schema object.
    fn from_root_schema(root_schema: &Schema) -> Result<Self> {
        let rust_type = root_schema
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing title from root"))?
            .to_string();
        let description = root_schema
            .get("description")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing description from {}", rust_type))?
            .to_string();
        Self::from_schema_object(rust_type, description, root_schema)
    }

    /// Construct a DataType from a non-root schema object.
    fn from_schema(rust_type: String, schema: &Schema) -> Result<Self> {
        let description = schema
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "no description".to_string());
        Self::from_schema_object(rust_type, description, schema)
    }

    /// Construct a DataType from a generic schema object.
    fn from_schema_object(rust_type: String, description: String, schema: &Schema) -> Result<Self> {
        // The description may be modified to add an error message.
        let mut description = description;

        // An enum.
        let inner = if let Some(enum_values) = schema.get("enum").and_then(|v| v.as_array()) {
            let variants = enum_values.iter().map(|v| v.to_string()).collect();
            DataTypeInner::Enum(EnumDataType { variants })
        } else if let Some(variants) = schema
            .get("oneOf")
            .and_then(|v| v.as_array())
            .and_then(|arr| extract_unit_variants_from_one_of(arr))
        {
            DataTypeInner::Enum(EnumDataType { variants })
        }
        // An enum with variants of different types.
        // TODO(b/332348955): Support this properly.
        // TODO(b/436293725): Support comments on enum variants.
        else if schema.get("oneOf").is_some()
            || schema.get("anyOf").is_some()
            || schema.get("allOf").is_some()
        {
            let error_message = format!(
                "Failed to generate docs for complex {} enum: b/332348955 or b/436293725",
                rust_type
            );
            description = format!("{}\n\n{}", error_message, description);
            // println!("{}", error_message);
            DataTypeInner::Enum(EnumDataType { variants: BTreeSet::new() })
        }
        // A struct.
        else if let Some(properties) = schema.get("properties").and_then(|v| v.as_object()) {
            let fields = properties
                .iter()
                .map(|(field_name, p)| {
                    let object: &Schema = p.try_into()?;
                    let data_type = if let Some(format) =
                        object.get("format").and_then(|v| v.as_str())
                    {
                        StructFieldType::Primitive { data_type: format.to_string() }
                    } else if let Some(reference) = object.get("$ref").and_then(|v| v.as_str()) {
                        StructFieldType::Custom { data_type: strip_ref_prefix(reference) }
                    } else if let Some(single_or_vec) = object.get("type") {
                        StructFieldType::Primitive {
                            data_type: single_or_vec_to_string(single_or_vec)?,
                        }
                    } else {
                        let mut subobjects = Vec::<&Value>::new();
                        if let Some(subs) = object.get("allOf").and_then(|v| v.as_array()) {
                            subobjects.extend(subs.iter());
                        }
                        if let Some(subs) = object.get("anyOf").and_then(|v| v.as_array()) {
                            subobjects.extend(subs.iter());
                        }
                        if let Some(subs) = object.get("oneOf").and_then(|v| v.as_array()) {
                            subobjects.extend(subs.iter());
                        }
                        if subobjects.is_empty() {
                            bail!("Missing subschemas for {}", rust_type);
                        }
                        let subobject = subobjects
                            .first()
                            .ok_or_else(|| anyhow!("Missing subobject for {}", rust_type))?;
                        let reference =
                            subobject.get("$ref").and_then(|v| v.as_str()).ok_or_else(|| {
                                anyhow!("Missing reference for field in {}", rust_type)
                            })?;
                        StructFieldType::Custom { data_type: strip_ref_prefix(reference) }
                    };
                    let description = object
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let default = object.get("default").cloned();
                    Ok(StructFieldData {
                        field_name: field_name.clone(),
                        data_type,
                        description,
                        default,
                    })
                })
                .collect::<Result<BTreeSet<StructFieldData>>>()?;
            DataTypeInner::Struct(StructDataType { fields })
        } else if schema.get("type").and_then(|v| v.as_str()) == Some("object")
            && (schema.get("additionalProperties").is_some()
                || schema.get("patternProperties").is_some())
        {
            DataTypeInner::Struct(StructDataType { fields: BTreeSet::new() })
        }
        // A primitive wrapped by a type.
        // e.g. ImageName(String)
        else if let Some(single_or_vec) = schema.get("type") {
            let data_type = single_or_vec_to_string(single_or_vec)?;
            DataTypeInner::Primitive(PrimitiveDataType { data_type })
        }
        // Unsupported.
        else {
            anyhow::bail!("Unsupported schema type for {}", rust_type);
        };

        Ok(Self { rust_type, description, inner })
    }
}

/// Convert a type Value (string or array of strings) to a user-friendly String.
fn single_or_vec_to_string(single_or_vec: &Value) -> Result<String> {
    match single_or_vec {
        Value::String(t) => Ok(instance_type_to_string(t)?),
        Value::Array(v) => {
            let t = v
                .first()
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("Missing instance type"))?;
            Ok(format!("[{}]", instance_type_to_string(t)?))
        }
        _ => bail!("unsupported type value"),
    }
}

/// Convert a schema InstanceType string to a user-friendly String.
fn instance_type_to_string(instance_type: &str) -> Result<String> {
    let s = match instance_type {
        "boolean" => "bool",
        "array" => "vector",
        "string" => "string",
        "integer" | "number" => "integer",
        "object" => "object",
        _ => bail!("unsupported type"),
    }
    .to_string();
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::{
        AllData, DataType, DataTypeInner, EnumDataType, StructDataType, StructFieldData,
        StructFieldType, single_or_vec_to_string,
    };
    use pretty_assertions::assert_eq;
    use schemars::JsonSchema;
    use schemars::generate::SchemaSettings;
    use serde::Serialize;
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};

    /// Mandatory description on root struct.
    #[derive(Serialize, JsonSchema)]
    struct RootStruct {
        /// Primitives should work
        field_1: u8,
        /// Nested enums should work
        field_2: MyEnum,
        /// Vectors should work
        field_3: Vec<String>,
        /// Nested structs should work
        field_4: MyStruct,
    }

    /// Really cool enum.
    #[allow(dead_code)]
    #[derive(Serialize, JsonSchema)]
    enum MyEnum {
        Variant1,
        #[serde(rename = "variant_2")]
        Variant2,
    }

    /// Really cool struct.
    #[derive(Serialize, JsonSchema)]
    struct MyStruct {
        /// Booleans should work
        field_5: bool,
    }

    #[test]
    fn test() {
        let settings = SchemaSettings::default();
        let generator = settings.into_generator();
        let root_schema = generator.into_root_schema_for::<RootStruct>();
        let all_data = AllData::from_root_schema(&"url_path".into(), &root_schema).unwrap();
        let expected = AllData {
            url_path: "url_path".into(),
            root: "RootStruct".into(),
            data_types: BTreeMap::from([
                (
                    "RootStruct".into(),
                    DataType {
                        rust_type: "RootStruct".into(),
                        description: "Mandatory description on root struct.".into(),
                        inner: DataTypeInner::Struct(StructDataType {
                            fields: BTreeSet::from([
                                StructFieldData {
                                    field_name: "field_1".into(),
                                    description: "Primitives should work".into(),
                                    default: None,
                                    data_type: StructFieldType::Primitive {
                                        data_type: "uint8".into(),
                                    },
                                },
                                StructFieldData {
                                    field_name: "field_2".into(),
                                    description: "Nested enums should work".into(),
                                    default: None,
                                    data_type: StructFieldType::Custom {
                                        data_type: "MyEnum".into(),
                                    },
                                },
                                StructFieldData {
                                    field_name: "field_3".into(),
                                    description: "Vectors should work".into(),
                                    default: None,
                                    data_type: StructFieldType::Primitive {
                                        data_type: "vector".into(),
                                    },
                                },
                                StructFieldData {
                                    field_name: "field_4".into(),
                                    description: "Nested structs should work".into(),
                                    default: None,
                                    data_type: StructFieldType::Custom {
                                        data_type: "MyStruct".into(),
                                    },
                                },
                            ]),
                        }),
                    },
                ),
                (
                    "MyEnum".into(),
                    DataType {
                        rust_type: "MyEnum".into(),
                        description: "Really cool enum.".into(),
                        inner: DataTypeInner::Enum(EnumDataType {
                            variants: BTreeSet::from([
                                "\"Variant1\"".into(),
                                "\"variant_2\"".into(),
                            ]),
                        }),
                    },
                ),
                (
                    "MyStruct".into(),
                    DataType {
                        rust_type: "MyStruct".into(),
                        description: "Really cool struct.".into(),
                        inner: DataTypeInner::Struct(StructDataType {
                            fields: BTreeSet::from([StructFieldData {
                                field_name: "field_5".into(),
                                description: "Booleans should work".into(),
                                default: None,
                                data_type: StructFieldType::Primitive {
                                    data_type: "boolean".into(),
                                },
                            }]),
                        }),
                    },
                ),
            ]),
        };
        assert_eq!(expected, all_data);
    }

    #[test]
    fn test_instance_type_to_string() {
        let s = single_or_vec_to_string(&json!("integer")).unwrap();
        assert_eq!("integer", &s);
        let s = single_or_vec_to_string(&json!("boolean")).unwrap();
        assert_eq!("bool", &s);
        let s = single_or_vec_to_string(&json!(["boolean"])).unwrap();
        assert_eq!("[bool]", &s);
    }
}

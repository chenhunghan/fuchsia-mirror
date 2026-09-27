// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::parser::Type;
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bound {
    Limit(u64),
    Max,
}

impl From<u64> for Bound {
    fn from(val: u64) -> Self {
        Bound::Limit(val)
    }
}

impl Serialize for Bound {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Bound::Limit(limit) => serializer.serialize_u64(*limit),
            Bound::Max => serializer.serialize_str("MAX"),
        }
    }
}

impl<'de> Deserialize<'de> for Bound {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundVisitor;

        impl<'de> Visitor<'de> for BoundVisitor {
            type Value = Bound;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a positive integer or \"MAX\"")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Bound, E>
            where
                E: de::Error,
            {
                Ok(Bound::Limit(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Bound, E>
            where
                E: de::Error,
            {
                if value >= 0 {
                    Ok(Bound::Limit(value as u64))
                } else {
                    Err(de::Error::invalid_value(de::Unexpected::Signed(value), &self))
                }
            }

            fn visit_u128<E>(self, value: u128) -> Result<Bound, E>
            where
                E: de::Error,
            {
                if let Ok(v) = u64::try_from(value) {
                    Ok(Bound::Limit(v))
                } else {
                    Err(de::Error::invalid_value(de::Unexpected::Other("u128 out of range"), &self))
                }
            }

            fn visit_i128<E>(self, value: i128) -> Result<Bound, E>
            where
                E: de::Error,
            {
                if value >= 0 {
                    if let Ok(v) = u64::try_from(value) {
                        Ok(Bound::Limit(v))
                    } else {
                        Err(de::Error::invalid_value(
                            de::Unexpected::Other("i128 out of range"),
                            &self,
                        ))
                    }
                } else {
                    Err(de::Error::invalid_value(de::Unexpected::Other("negative integer"), &self))
                }
            }

            fn visit_f64<E>(self, value: f64) -> Result<Bound, E>
            where
                E: de::Error,
            {
                if value >= 0.0 && value.fract() == 0.0 && value <= u64::MAX as f64 {
                    Ok(Bound::Limit(value as u64))
                } else {
                    Err(de::Error::invalid_value(de::Unexpected::Float(value), &self))
                }
            }

            fn visit_str<E>(self, value: &str) -> Result<Bound, E>
            where
                E: de::Error,
            {
                if value == "MAX" {
                    Ok(Bound::Max)
                } else if let Ok(parsed) = value.parse::<u64>() {
                    Ok(Bound::Limit(parsed))
                } else {
                    Err(de::Error::invalid_value(de::Unexpected::Str(value), &self))
                }
            }
        }

        deserializer.deserialize_any(BoundVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OneOfList(pub Vec<Vec<String>>);

impl Serialize for OneOfList {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for OneOfList {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OneOfVisitor;

        impl<'de> Visitor<'de> for OneOfVisitor {
            type Value = OneOfList;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a list of strings or a list of lists of strings")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<OneOfList, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                let mut result = Vec::new();
                let mut single_group = Vec::new();
                let mut is_1d = false;
                let mut is_2d = false;

                while let Some(elem) = seq.next_element::<Value>()? {
                    match elem {
                        Value::String(s) => {
                            if is_2d {
                                return Err(de::Error::custom(
                                    "one_of cannot mix strings and lists of strings",
                                ));
                            }
                            is_1d = true;
                            single_group.push(s);
                        }
                        Value::Array(arr) => {
                            if is_1d {
                                return Err(de::Error::custom(
                                    "one_of cannot mix strings and lists of strings",
                                ));
                            }
                            is_2d = true;
                            let group: Result<Vec<String>, _> = arr
                                .into_iter()
                                .map(|v| match v {
                                    Value::String(s) => Ok(s),
                                    _ => Err(de::Error::custom("one_of elements must be strings")),
                                })
                                .collect();
                            result.push(group?);
                        }
                        _ => {
                            return Err(de::Error::custom(
                                "one_of must contain strings or lists of strings",
                            ));
                        }
                    }
                }

                if is_1d { Ok(OneOfList(vec![single_group])) } else { Ok(OneOfList(result)) }
            }
        }

        deserializer.deserialize_seq(OneOfVisitor)
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Default)]
pub struct PropertyDef {
    #[serde(rename = "type", default)]
    pub property_type: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub max_size: Option<Bound>,
    #[serde(default)]
    pub min_size: Option<u64>,
    #[serde(default)]
    pub max_count: Option<Bound>,
    #[serde(default)]
    pub min_count: Option<u64>,
    #[serde(default)]
    pub min_inclusive: Option<i64>,
    #[serde(default)]
    pub max_inclusive: Option<i64>,
    #[serde(default)]
    pub min_exclusive: Option<i64>,
    #[serde(default)]
    pub max_exclusive: Option<i64>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub underlying_type: Option<String>,
    #[serde(default)]
    pub bits: Option<Value>,
    #[serde(rename = "ref", alias = "$ref", default)]
    pub reference: Option<String>,
    #[serde(default, alias = "items")]
    pub element: Option<Box<PropertyDef>>,
    #[serde(default)]
    pub properties: Option<HashMap<String, PropertyDef>>,
    #[serde(default)]
    pub required: Option<Vec<String>>,
    #[serde(default, alias = "oneOf", alias = "one_of")]
    pub one_of: Option<OneOfList>,
    #[serde(default, rename = "enum")]
    pub enum_values: Option<Value>,
    #[serde(default, rename = "const")]
    pub const_value: Option<Value>,
    #[serde(default)]
    pub optional: Option<bool>,
}

impl PropertyDef {
    pub fn validate(&self, name: &str) -> Result<(), anyhow::Error> {
        if let Some(r) = &self.reference {
            if r.trim().is_empty() {
                anyhow::bail!("Property '{}' has an empty 'ref'", name);
            }
            if self.max_size.is_some() {
                anyhow::bail!("Property '{}' with 'ref' cannot specify 'max_size'", name);
            }
            if self.min_size.is_some() {
                anyhow::bail!("Property '{}' with 'ref' cannot specify 'min_size'", name);
            }
            if self.max_count.is_some() {
                anyhow::bail!("Property '{}' with 'ref' cannot specify 'max_count'", name);
            }
            if self.min_count.is_some() {
                anyhow::bail!("Property '{}' with 'ref' cannot specify 'min_count'", name);
            }
            if self.element.is_some() {
                anyhow::bail!("Property '{}' with 'ref' cannot specify 'element'", name);
            }
            if self.properties.is_some() {
                anyhow::bail!("Property '{}' with 'ref' cannot specify 'properties'", name);
            }
            if self.underlying_type.is_some() {
                anyhow::bail!("Property '{}' with 'ref' cannot specify 'underlying_type'", name);
            }
            if self.bits.is_some() {
                anyhow::bail!("Property '{}' with 'ref' cannot specify 'bits'", name);
            }
            if self.required.is_some() {
                anyhow::bail!("Property '{}' with 'ref' cannot specify 'required'", name);
            }
            if self.one_of.is_some() {
                anyhow::bail!("Property '{}' with 'ref' cannot specify 'one_of'", name);
            }
            if let Some(property_type) = self.property_type.as_deref() {
                if property_type != "ref" && property_type != "object" {
                    anyhow::bail!(
                        "Property '{}' specifies both 'ref' and incompatible type '{}'",
                        name,
                        property_type
                    );
                }
            }
            return Ok(());
        }

        // If it is a constant definition in defs
        if self.const_value.is_some() {
            let Some(type_str) = self.property_type.as_deref() else {
                anyhow::bail!("Constant '{}' must specify a 'type'", name);
            };
            match type_str {
                "bool" | "uint8" | "uint16" | "uint32" | "uint64" | "int8" | "int16" | "int32"
                | "int64" | "string" => {}
                _ => anyhow::bail!("Constant '{}' has unsupported type '{}'", name, type_str),
            }
            return Ok(());
        }

        let Some(type_str) = self.property_type.as_deref() else {
            anyhow::bail!("Property '{}' must specify a 'type' or 'ref'", name);
        };

        match type_str {
            "bool" => {
                if self.max_size.is_some() {
                    anyhow::bail!("Property '{}' of type 'bool' cannot specify 'max_size'", name);
                }
                if self.min_size.is_some() {
                    anyhow::bail!("Property '{}' of type 'bool' cannot specify 'min_size'", name);
                }
                if self.max_count.is_some() {
                    anyhow::bail!("Property '{}' of type 'bool' cannot specify 'max_count'", name);
                }
                if self.min_count.is_some() {
                    anyhow::bail!("Property '{}' of type 'bool' cannot specify 'min_count'", name);
                }
                if self.element.is_some() {
                    anyhow::bail!("Property '{}' of type 'bool' cannot specify 'element'", name);
                }
                if self.properties.is_some() {
                    anyhow::bail!("Property '{}' of type 'bool' cannot specify 'properties'", name);
                }
                if self.pattern.is_some() {
                    anyhow::bail!("Property '{}' of type 'bool' cannot specify 'pattern'", name);
                }
                if self.underlying_type.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type 'bool' cannot specify 'underlying_type'",
                        name
                    );
                }
                if self.bits.is_some() {
                    anyhow::bail!("Property '{}' of type 'bool' cannot specify 'bits'", name);
                }
                if self.required.is_some() {
                    anyhow::bail!("Property '{}' of type 'bool' cannot specify 'required'", name);
                }
                if self.one_of.is_some() {
                    anyhow::bail!("Property '{}' of type 'bool' cannot specify 'one_of'", name);
                }
                if self.min_inclusive.is_some()
                    || self.max_inclusive.is_some()
                    || self.min_exclusive.is_some()
                    || self.max_exclusive.is_some()
                {
                    anyhow::bail!("Property '{}' of type 'bool' cannot specify range fields", name);
                }
            }
            "uint8" | "uint16" | "uint32" | "uint64" | "int8" | "int16" | "int32" | "int64" => {
                if self.max_size.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type '{}' cannot specify 'max_size'",
                        name,
                        type_str
                    );
                }
                if self.min_size.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type '{}' cannot specify 'min_size'",
                        name,
                        type_str
                    );
                }
                if self.max_count.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type '{}' cannot specify 'max_count'",
                        name,
                        type_str
                    );
                }
                if self.min_count.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type '{}' cannot specify 'min_count'",
                        name,
                        type_str
                    );
                }
                if self.element.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type '{}' cannot specify 'element'",
                        name,
                        type_str
                    );
                }
                if self.properties.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type '{}' cannot specify 'properties'",
                        name,
                        type_str
                    );
                }
                if self.pattern.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type '{}' cannot specify 'pattern'",
                        name,
                        type_str
                    );
                }
                if self.underlying_type.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type '{}' cannot specify 'underlying_type'",
                        name,
                        type_str
                    );
                }
                if self.bits.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type '{}' cannot specify 'bits'",
                        name,
                        type_str
                    );
                }
                if self.required.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type '{}' cannot specify 'required'",
                        name,
                        type_str
                    );
                }
                if self.one_of.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type '{}' cannot specify 'one_of'",
                        name,
                        type_str
                    );
                }
                if self.min_inclusive.is_some() && self.min_exclusive.is_some() {
                    anyhow::bail!(
                        "Property '{}' cannot specify both 'min_inclusive' and 'min_exclusive'",
                        name
                    );
                }
                if self.max_inclusive.is_some() && self.max_exclusive.is_some() {
                    anyhow::bail!(
                        "Property '{}' cannot specify both 'max_inclusive' and 'max_exclusive'",
                        name
                    );
                }
            }
            "string" => {
                if self.max_count.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type 'string' cannot specify 'max_count'",
                        name
                    );
                }
                if self.min_count.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type 'string' cannot specify 'min_count'",
                        name
                    );
                }
                if self.element.is_some() {
                    anyhow::bail!("Property '{}' of type 'string' cannot specify 'element'", name);
                }
                if self.properties.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type 'string' cannot specify 'properties'",
                        name
                    );
                }
                if self.underlying_type.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type 'string' cannot specify 'underlying_type'",
                        name
                    );
                }
                if self.bits.is_some() {
                    anyhow::bail!("Property '{}' of type 'string' cannot specify 'bits'", name);
                }
                if self.required.is_some() {
                    anyhow::bail!("Property '{}' of type 'string' cannot specify 'required'", name);
                }
                if self.one_of.is_some() {
                    anyhow::bail!("Property '{}' of type 'string' cannot specify 'one_of'", name);
                }
                if self.min_inclusive.is_some()
                    || self.max_inclusive.is_some()
                    || self.min_exclusive.is_some()
                    || self.max_exclusive.is_some()
                {
                    anyhow::bail!(
                        "Property '{}' of type 'string' cannot specify range fields",
                        name
                    );
                }
                if self.enum_values.is_some() && self.min_size.is_some() {
                    anyhow::bail!(
                        "Property '{}' cannot specify 'min_size' when 'enum' is defined",
                        name
                    );
                }
                if let Some(ms) = &self.max_size {
                    if let Bound::Limit(0) = ms {
                        anyhow::bail!("Property '{}' 'max_size' must be non-zero", name);
                    }
                } else if self.enum_values.is_none() {
                    anyhow::bail!("Property '{}' of type 'string' must specify 'max_size'", name);
                }
                if let Some(mins) = self.min_size {
                    if mins == 0 {
                        anyhow::bail!("Property '{}' 'min_size' must be non-zero", name);
                    }
                    if let Some(Bound::Limit(maxs)) = self.max_size {
                        if mins > maxs {
                            anyhow::bail!(
                                "Property '{}' 'min_size' ({}) cannot exceed 'max_size' ({})",
                                name,
                                mins,
                                maxs
                            );
                        }
                    }
                }
            }
            "vector" => {
                let Some(elem) = &self.element else {
                    anyhow::bail!("Property '{}' of type 'vector' must specify 'element'", name);
                };
                if self.max_size.is_some() {
                    anyhow::bail!("Property '{}' of type 'vector' cannot specify 'max_size'", name);
                }
                if self.min_size.is_some() {
                    anyhow::bail!("Property '{}' of type 'vector' cannot specify 'min_size'", name);
                }
                if self.pattern.is_some() {
                    anyhow::bail!("Property '{}' of type 'vector' cannot specify 'pattern'", name);
                }
                if self.properties.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type 'vector' cannot specify 'properties'",
                        name
                    );
                }
                if self.underlying_type.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type 'vector' cannot specify 'underlying_type'",
                        name
                    );
                }
                if self.bits.is_some() {
                    anyhow::bail!("Property '{}' of type 'vector' cannot specify 'bits'", name);
                }
                if self.required.is_some() {
                    anyhow::bail!("Property '{}' of type 'vector' cannot specify 'required'", name);
                }
                if self.one_of.is_some() {
                    anyhow::bail!("Property '{}' of type 'vector' cannot specify 'one_of'", name);
                }
                if self.min_inclusive.is_some()
                    || self.max_inclusive.is_some()
                    || self.min_exclusive.is_some()
                    || self.max_exclusive.is_some()
                {
                    anyhow::bail!(
                        "Property '{}' of type 'vector' cannot specify range fields",
                        name
                    );
                }
                if let Some(mc) = &self.max_count {
                    if let Bound::Limit(0) = mc {
                        anyhow::bail!("Property '{}' 'max_count' must be non-zero", name);
                    }
                } else {
                    anyhow::bail!("Property '{}' of type 'vector' must specify 'max_count'", name);
                }
                if let Some(minc) = self.min_count {
                    if minc == 0 {
                        anyhow::bail!("Property '{}' 'min_count' must be non-zero", name);
                    }
                    if let Some(Bound::Limit(maxc)) = self.max_count {
                        if minc > maxc {
                            anyhow::bail!(
                                "Property '{}' 'min_count' ({}) cannot exceed 'max_count' ({})",
                                name,
                                minc,
                                maxc
                            );
                        }
                    }
                }
                elem.validate(&format!("{}.element", name))?;
            }
            "object" => {
                if self.max_size.is_some() {
                    anyhow::bail!("Property '{}' of type 'object' cannot specify 'max_size'", name);
                }
                if self.min_size.is_some() {
                    anyhow::bail!("Property '{}' of type 'object' cannot specify 'min_size'", name);
                }
                if self.max_count.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type 'object' cannot specify 'max_count'",
                        name
                    );
                }
                if self.min_count.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type 'object' cannot specify 'min_count'",
                        name
                    );
                }
                if self.element.is_some() {
                    anyhow::bail!("Property '{}' of type 'object' cannot specify 'element'", name);
                }
                if self.pattern.is_some() {
                    anyhow::bail!("Property '{}' of type 'object' cannot specify 'pattern'", name);
                }
                if self.underlying_type.is_some() {
                    anyhow::bail!(
                        "Property '{}' of type 'object' cannot specify 'underlying_type'",
                        name
                    );
                }
                if self.bits.is_some() {
                    anyhow::bail!("Property '{}' of type 'object' cannot specify 'bits'", name);
                }
                if self.min_inclusive.is_some()
                    || self.max_inclusive.is_some()
                    || self.min_exclusive.is_some()
                    || self.max_exclusive.is_some()
                {
                    anyhow::bail!(
                        "Property '{}' of type 'object' cannot specify range fields",
                        name
                    );
                }
                if let Some(props) = &self.properties {
                    for (child_name, child_prop) in props {
                        child_prop.validate(&format!("{}.{}", name, child_name))?;
                    }
                    if let Some(req_list) = &self.required {
                        for req in req_list {
                            if !props.contains_key(req) {
                                anyhow::bail!(
                                    "Object property '{}' requires '{}' which is not in properties",
                                    name,
                                    req
                                );
                            }
                        }
                    }
                    if let Some(one_of_list) = &self.one_of {
                        for group in &one_of_list.0 {
                            for prop in group {
                                if !props.contains_key(prop) {
                                    anyhow::bail!(
                                        "Object property '{}' specifies '{}' in one_of which is not in properties",
                                        name,
                                        prop
                                    );
                                }
                            }
                        }
                    }
                }
            }
            "bits" => {
                if self.max_size.is_some() {
                    anyhow::bail!("Property '{}' of type 'bits' cannot specify 'max_size'", name);
                }
                if self.min_size.is_some() {
                    anyhow::bail!("Property '{}' of type 'bits' cannot specify 'min_size'", name);
                }
                if self.max_count.is_some() {
                    anyhow::bail!("Property '{}' of type 'bits' cannot specify 'max_count'", name);
                }
                if self.min_count.is_some() {
                    anyhow::bail!("Property '{}' of type 'bits' cannot specify 'min_count'", name);
                }
                if self.element.is_some() {
                    anyhow::bail!("Property '{}' of type 'bits' cannot specify 'element'", name);
                }
                if self.pattern.is_some() {
                    anyhow::bail!("Property '{}' of type 'bits' cannot specify 'pattern'", name);
                }
                if self.properties.is_some() {
                    anyhow::bail!("Property '{}' of type 'bits' cannot specify 'properties'", name);
                }
                if self.required.is_some() {
                    anyhow::bail!("Property '{}' of type 'bits' cannot specify 'required'", name);
                }
                if self.one_of.is_some() {
                    anyhow::bail!("Property '{}' of type 'bits' cannot specify 'one_of'", name);
                }
                if self.min_inclusive.is_some()
                    || self.max_inclusive.is_some()
                    || self.min_exclusive.is_some()
                    || self.max_exclusive.is_some()
                {
                    anyhow::bail!("Property '{}' of type 'bits' cannot specify range fields", name);
                }
                let Some(ut) = self.underlying_type.as_deref() else {
                    anyhow::bail!(
                        "Property '{}' of type 'bits' must specify 'underlying_type'",
                        name
                    );
                };
                match ut {
                    "uint8" | "uint16" | "uint32" | "uint64" | "int8" | "int16" | "int32"
                    | "int64" => {}
                    _ => anyhow::bail!(
                        "Property '{}' of type 'bits' has unsupported underlying_type '{}'",
                        name,
                        ut
                    ),
                }
            }
            _ => anyhow::bail!("Property '{}' has unsupported type '{}'", name, type_str),
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn to_type(&self) -> Result<Type, anyhow::Error> {
        if let Some(r) = &self.reference {
            let name = r.split('/').next_back().unwrap_or(r);
            return Ok(Type::Struct(name.to_string()));
        }
        let Some(type_str) = self.property_type.as_deref() else {
            anyhow::bail!("PropertyDef has neither 'type' nor 'ref': {:?}", self);
        };
        match type_str {
            "bool" => Ok(Type::Bool),
            "string" => Ok(Type::String),
            "uint8" => Ok(Type::Uint8),
            "uint16" => Ok(Type::Uint16),
            "uint32" => Ok(Type::Uint32),
            "uint64" => Ok(Type::Uint64),
            "int8" => Ok(Type::Int8),
            "int16" => Ok(Type::Int16),
            "int32" => Ok(Type::Int32),
            "int64" => Ok(Type::Int64),
            "vector" => {
                let elem = self.element.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("Missing 'element' or 'items' in vector property")
                })?;
                let elem_ty = elem.to_type()?;
                Ok(Type::Vector(Box::new(elem_ty)))
            }
            "object" => Ok(Type::Struct("Object".to_string())),
            "bits" => {
                let ut = self.underlying_type.as_deref().unwrap_or("uint32");
                match ut {
                    "uint8" => Ok(Type::Uint8),
                    "uint16" => Ok(Type::Uint16),
                    "uint32" => Ok(Type::Uint32),
                    "uint64" => Ok(Type::Uint64),
                    "int8" => Ok(Type::Int8),
                    "int16" => Ok(Type::Int16),
                    "int32" => Ok(Type::Int32),
                    "int64" => Ok(Type::Int64),
                    _ => anyhow::bail!("Unsupported underlying_type for bits: {}", ut),
                }
            }
            _ => anyhow::bail!("Unsupported property type: {}", type_str),
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Default)]
pub struct DriverConfigDef {
    pub driver_config: String,
    #[serde(default)]
    pub defs: Option<HashMap<String, PropertyDef>>,
    #[serde(default)]
    pub properties: HashMap<String, PropertyDef>,
    #[serde(default)]
    pub required: Option<Vec<String>>,
    #[serde(default, alias = "oneOf", alias = "one_of")]
    pub one_of: Option<OneOfList>,
}

impl DriverConfigDef {
    pub fn validate(&self) -> Result<(), anyhow::Error> {
        if self.driver_config.trim().is_empty() {
            anyhow::bail!("Driver configuration definition cannot have an empty 'driver_config'");
        }
        if let Some(defs) = &self.defs {
            for (name, def) in defs {
                def.validate(&format!("defs.{}", name))?;
            }
        }
        for (name, prop) in &self.properties {
            prop.validate(name)?;
        }
        if let Some(req_list) = &self.required {
            for req in req_list {
                if !self.properties.contains_key(req) {
                    anyhow::bail!(
                        "Required property '{}' is not defined in properties of driver configuration '{}'",
                        req,
                        self.driver_config
                    );
                }
            }
        }
        if let Some(one_of_list) = &self.one_of {
            for group in &one_of_list.0 {
                for prop in group {
                    if !self.properties.contains_key(prop) {
                        anyhow::bail!(
                            "Property '{}' in one_of is not defined in properties of driver configuration '{}'",
                            prop,
                            self.driver_config
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Default)]
pub struct ServiceConstraintDef {
    pub service: String,
    #[serde(default)]
    pub defs: Option<HashMap<String, PropertyDef>>,
    #[serde(default, alias = "properties")]
    pub constraints: HashMap<String, PropertyDef>,
    #[serde(default)]
    pub required: Option<Vec<String>>,
    #[serde(default, alias = "oneOf", alias = "one_of")]
    pub one_of: Option<OneOfList>,
}

impl ServiceConstraintDef {
    pub fn validate(&self) -> Result<(), anyhow::Error> {
        if self.service.trim().is_empty() {
            anyhow::bail!("Service constraint definition cannot have an empty 'service'");
        }
        if let Some(defs) = &self.defs {
            for (name, def) in defs {
                def.validate(&format!("defs.{}", name))?;
            }
        }
        for (name, prop) in &self.constraints {
            prop.validate(name)?;
        }
        if let Some(req_list) = &self.required {
            for req in req_list {
                if !self.constraints.contains_key(req) {
                    anyhow::bail!(
                        "Required property '{}' is not defined in constraints of service '{}'",
                        req,
                        self.service
                    );
                }
            }
        }
        if let Some(one_of_list) = &self.one_of {
            for group in &one_of_list.0 {
                for prop in group {
                    if !self.constraints.contains_key(prop) {
                        anyhow::bail!(
                            "Property '{}' in one_of is not defined in constraints of service '{}'",
                            prop,
                            self.service
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json5;

    #[test]
    fn test_string_property_validation() {
        // Valid string
        let valid_prop = PropertyDef {
            property_type: Some("string".to_string()),
            max_size: Some(Bound::Limit(64)),
            ..Default::default()
        };
        assert!(valid_prop.validate("my_string").is_ok());

        // String missing max_size
        let missing_max_size =
            PropertyDef { property_type: Some("string".to_string()), ..Default::default() };
        let err = missing_max_size.validate("my_string").unwrap_err();
        assert!(err.to_string().contains("must specify 'max_size'"));

        // String with max_count
        let with_max_count = PropertyDef {
            property_type: Some("string".to_string()),
            max_size: Some(Bound::Limit(64)),
            max_count: Some(Bound::Limit(10)),
            ..Default::default()
        };
        let err = with_max_count.validate("my_string").unwrap_err();
        assert!(err.to_string().contains("cannot specify 'max_count'"));

        // String with element
        let with_element = PropertyDef {
            property_type: Some("string".to_string()),
            max_size: Some(Bound::Limit(64)),
            element: Some(Box::new(PropertyDef {
                property_type: Some("uint32".to_string()),
                ..Default::default()
            })),
            ..Default::default()
        };
        let err = with_element.validate("my_string").unwrap_err();
        assert!(err.to_string().contains("cannot specify 'element'"));
    }

    #[test]
    fn test_vector_property_validation() {
        // Valid vector
        let valid_prop = PropertyDef {
            property_type: Some("vector".to_string()),
            max_count: Some(Bound::Limit(100)),
            element: Some(Box::new(PropertyDef {
                reference: Some("fuchsia.hardware.pinimpl.Metadata/DevicePinStates".to_string()),
                ..Default::default()
            })),
            ..Default::default()
        };
        assert!(valid_prop.validate("my_vector").is_ok());

        // Vector missing max_count
        let missing_max_count = PropertyDef {
            property_type: Some("vector".to_string()),
            element: Some(Box::new(PropertyDef {
                reference: Some("fuchsia.hardware.pinimpl.Metadata/DevicePinStates".to_string()),
                ..Default::default()
            })),
            ..Default::default()
        };
        let err = missing_max_count.validate("my_vector").unwrap_err();
        assert!(err.to_string().contains("must specify 'max_count'"));

        // Vector missing element
        let missing_element = PropertyDef {
            property_type: Some("vector".to_string()),
            max_count: Some(Bound::Limit(100)),
            ..Default::default()
        };
        let err = missing_element.validate("my_vector").unwrap_err();
        assert!(err.to_string().contains("must specify 'element'"));

        // Vector with max_size
        let with_max_size = PropertyDef {
            property_type: Some("vector".to_string()),
            max_count: Some(Bound::Limit(100)),
            max_size: Some(Bound::Limit(64)),
            element: Some(Box::new(PropertyDef {
                reference: Some("fuchsia.hardware.pinimpl.Metadata/DevicePinStates".to_string()),
                ..Default::default()
            })),
            ..Default::default()
        };
        let err = with_max_size.validate("my_vector").unwrap_err();
        assert!(err.to_string().contains("cannot specify 'max_size'"));

        // Vector with invalid nested element (e.g. string without max_size)
        let invalid_nested = PropertyDef {
            property_type: Some("vector".to_string()),
            max_count: Some(Bound::Limit(100)),
            element: Some(Box::new(PropertyDef {
                property_type: Some("string".to_string()),
                ..Default::default()
            })),
            ..Default::default()
        };
        let err = invalid_nested.validate("my_vector").unwrap_err();
        assert!(err.to_string().contains("must specify 'max_size'"));
    }

    #[test]
    fn test_scalar_and_ref_property_validation() {
        // Valid scalar
        let valid_scalar =
            PropertyDef { property_type: Some("uint32".to_string()), ..Default::default() };
        assert!(valid_scalar.validate("pin").is_ok());

        // Scalar with max_size
        let scalar_with_size = PropertyDef {
            property_type: Some("uint32".to_string()),
            max_size: Some(Bound::Limit(4)),
            ..Default::default()
        };
        let err = scalar_with_size.validate("pin").unwrap_err();
        assert!(err.to_string().contains("cannot specify 'max_size'"));

        // Scalar with max_count
        let scalar_with_count = PropertyDef {
            property_type: Some("uint32".to_string()),
            max_count: Some(Bound::Limit(10)),
            ..Default::default()
        };
        let err = scalar_with_count.validate("pin").unwrap_err();
        assert!(err.to_string().contains("cannot specify 'max_count'"));

        // Valid ref
        let valid_ref = PropertyDef {
            reference: Some("fuchsia.hardware.pinimpl.Metadata/DevicePinStates".to_string()),
            ..Default::default()
        };
        assert!(valid_ref.validate("state").is_ok());

        // Ref with max_size
        let ref_with_size = PropertyDef {
            reference: Some("fuchsia.hardware.pinimpl.Metadata/DevicePinStates".to_string()),
            max_size: Some(Bound::Limit(64)),
            ..Default::default()
        };
        let err = ref_with_size.validate("state").unwrap_err();
        assert!(err.to_string().contains("cannot specify 'max_size'"));
    }

    #[test]
    fn test_object_and_bits_validation() {
        // Valid object
        let mut obj_props = HashMap::new();
        obj_props.insert(
            "frequency".to_string(),
            PropertyDef { property_type: Some("uint32".to_string()), ..Default::default() },
        );
        let valid_obj = PropertyDef {
            property_type: Some("object".to_string()),
            properties: Some(obj_props),
            required: Some(vec!["frequency".to_string()]),
            ..Default::default()
        };
        assert!(valid_obj.validate("my_obj").is_ok());

        // Object with missing required property
        let invalid_obj_req = PropertyDef {
            property_type: Some("object".to_string()),
            properties: Some(HashMap::new()),
            required: Some(vec!["non_existent".to_string()]),
            ..Default::default()
        };
        let err = invalid_obj_req.validate("my_obj").unwrap_err();
        assert!(err.to_string().contains("requires 'non_existent'"));

        // Valid bits
        let valid_bits = PropertyDef {
            property_type: Some("bits".to_string()),
            underlying_type: Some("uint64".to_string()),
            ..Default::default()
        };
        assert!(valid_bits.validate("disable_mask").is_ok());

        // Bits missing underlying_type
        let missing_ut =
            PropertyDef { property_type: Some("bits".to_string()), ..Default::default() };
        let err = missing_ut.validate("disable_mask").unwrap_err();
        assert!(err.to_string().contains("must specify 'underlying_type'"));

        // Bits with unsupported underlying_type
        let invalid_ut = PropertyDef {
            property_type: Some("bits".to_string()),
            underlying_type: Some("string".to_string()),
            ..Default::default()
        };
        let err = invalid_ut.validate("disable_mask").unwrap_err();
        assert!(err.to_string().contains("unsupported underlying_type"));
    }

    #[test]
    fn test_integer_range_and_defs_validation() {
        // Integer with conflicting min
        let conflicting_min = PropertyDef {
            property_type: Some("uint32".to_string()),
            min_inclusive: Some(0),
            min_exclusive: Some(0),
            ..Default::default()
        };
        let err = conflicting_min.validate("size").unwrap_err();
        assert!(
            err.to_string().contains("cannot specify both 'min_inclusive' and 'min_exclusive'")
        );

        // Integer with conflicting max
        let conflicting_max = PropertyDef {
            property_type: Some("uint32".to_string()),
            max_inclusive: Some(10),
            max_exclusive: Some(10),
            ..Default::default()
        };
        let err = conflicting_max.validate("size").unwrap_err();
        assert!(
            err.to_string().contains("cannot specify both 'max_inclusive' and 'max_exclusive'")
        );

        // DriverConfigDef with valid defs and const
        let mut defs = HashMap::new();
        defs.insert(
            "MaxPinLength".to_string(),
            PropertyDef {
                property_type: Some("uint64".to_string()),
                const_value: Some(Value::Number(64.into())),
                ..Default::default()
            },
        );
        let mut properties = HashMap::new();
        properties.insert(
            "pin".to_string(),
            PropertyDef { property_type: Some("uint32".to_string()), ..Default::default() },
        );
        let driver_config = DriverConfigDef {
            driver_config: "fuchsia.hardware.pinimpl.Config".to_string(),
            defs: Some(defs),
            properties,
            required: Some(vec!["pin".to_string()]),
            one_of: None,
        };
        assert!(driver_config.validate().is_ok());
    }

    #[test]
    fn test_driver_configs_and_service_constraints() {
        // Empty driver_config
        let empty_config = DriverConfigDef {
            driver_config: "".to_string(),
            defs: None,
            properties: HashMap::new(),
            required: None,
            one_of: None,
        };
        assert!(
            empty_config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("cannot have an empty 'driver_config'")
        );

        // Empty service
        let empty_service = ServiceConstraintDef {
            service: "".to_string(),
            defs: None,
            constraints: HashMap::new(),
            required: None,
            one_of: None,
        };
        assert!(
            empty_service
                .validate()
                .unwrap_err()
                .to_string()
                .contains("cannot have an empty 'service'")
        );
    }

    #[test]
    fn test_bound_max_parsing_and_validation() {
        let sample = r#"{
  driver_config: "fuchsia.hardware.test.Config",
  properties: {
    unbounded_string: {
      type: "string",
      max_size: "MAX",
      min_size: 3
    },
    unbounded_vector: {
      type: "vector",
      element: {
        type: "uint32"
      },
      max_count: "MAX",
      min_count: 1
    }
  }
}"#;
        let config_def: DriverConfigDef =
            serde_json5::from_str(sample).expect("Failed to parse MAX bounds");
        let string_prop = config_def.properties.get("unbounded_string").unwrap();
        assert_eq!(string_prop.max_size, Some(Bound::Max));
        assert_eq!(string_prop.min_size, Some(3));
        assert!(string_prop.validate("unbounded_string").is_ok());

        let vector_prop = config_def.properties.get("unbounded_vector").unwrap();
        assert_eq!(vector_prop.max_count, Some(Bound::Max));
        assert_eq!(vector_prop.min_count, Some(1));
        assert!(vector_prop.validate("unbounded_vector").is_ok());

        assert!(config_def.validate().is_ok());
    }

    #[test]
    fn test_bound_invalid_values() {
        // Invalid string for max_size
        let invalid_max_size_str = r#"{
  type: "string",
  max_size: "INVALID"
}"#;
        let res: Result<PropertyDef, _> = serde_json5::from_str(invalid_max_size_str);
        assert!(res.is_err());

        // Zero limit should fail validation
        let zero_max_size = PropertyDef {
            property_type: Some("string".to_string()),
            max_size: Some(Bound::Limit(0)),
            ..Default::default()
        };
        assert!(zero_max_size.validate("str").is_err());

        let zero_max_count = PropertyDef {
            property_type: Some("vector".to_string()),
            element: Some(Box::new(PropertyDef {
                property_type: Some("uint32".to_string()),
                ..Default::default()
            })),
            max_count: Some(Bound::Limit(0)),
            ..Default::default()
        };
        assert!(zero_max_count.validate("vec").is_err());
    }

    #[test]
    fn test_bound_serialization() {
        assert_eq!(serde_json::to_string(&Bound::Limit(120)).unwrap(), "120");
        assert_eq!(serde_json::to_string(&Bound::Max).unwrap(), "\"MAX\"");
    }

    #[test]
    fn test_one_of_and_required_object_property() {
        let sample = r#"{
  type: "object",
  properties: {
    direct: { type: "object", properties: {} },
    matrix: { type: "object", properties: {} },
    id: { type: "string", max_size: 64 }
  },
  required: ["id"],
  oneOf: [["direct", "matrix"]]
}"#;
        let prop: PropertyDef =
            serde_json5::from_str(sample).expect("Failed to parse object with oneOf and required");
        assert_eq!(prop.required, Some(vec!["id".to_string()]));
        assert_eq!(
            prop.one_of,
            Some(OneOfList(vec![vec!["direct".to_string(), "matrix".to_string()]]))
        );
        assert!(prop.validate("button_config").is_ok());

        // one_of with invalid property name
        let invalid_sample = r#"{
  type: "object",
  properties: {
    direct: { type: "object", properties: {} }
  },
  one_of: [["direct", "non_existent"]]
}"#;
        let invalid_prop: PropertyDef = serde_json5::from_str(invalid_sample).unwrap();
        let err = invalid_prop.validate("button_config").unwrap_err();
        assert!(err.to_string().contains("specifies 'non_existent' in one_of"));
    }

    #[test]
    fn test_one_of_1d_and_2d_formats() {
        // 1D format
        let sample_1d = r#"{
  driver_config: "fuchsia.test.Config",
  properties: {
    opt_a: { type: "uint32" },
    opt_b: { type: "uint32" }
  },
  one_of: ["opt_a", "opt_b"]
}"#;
        let config_1d: DriverConfigDef = serde_json5::from_str(sample_1d).unwrap();
        assert_eq!(
            config_1d.one_of,
            Some(OneOfList(vec![vec!["opt_a".to_string(), "opt_b".to_string()]]))
        );
        assert!(config_1d.validate().is_ok());

        // 2D format with oneOf
        let sample_2d = r#"{
  service: "fuchsia.test.Service",
  constraints: {
    opt_a: { type: "uint32" },
    opt_b: { type: "uint32" },
    opt_c: { type: "string", max_size: 10 },
    opt_d: { type: "string", max_size: 10 }
  },
  oneOf: [
    ["opt_a", "opt_b"],
    ["opt_c", "opt_d"]
  ]
}"#;
        let constraint_2d: ServiceConstraintDef = serde_json5::from_str(sample_2d).unwrap();
        assert_eq!(
            constraint_2d.one_of,
            Some(OneOfList(vec![
                vec!["opt_a".to_string(), "opt_b".to_string()],
                vec!["opt_c".to_string(), "opt_d".to_string()]
            ]))
        );
        assert!(constraint_2d.validate().is_ok());
    }

    #[test]
    fn test_non_object_properties_reject_required_and_one_of() {
        let scalar_with_required = PropertyDef {
            property_type: Some("uint32".to_string()),
            required: Some(vec!["field".to_string()]),
            ..Default::default()
        };
        assert!(
            scalar_with_required
                .validate("scalar")
                .unwrap_err()
                .to_string()
                .contains("cannot specify 'required'")
        );

        let string_with_one_of = PropertyDef {
            property_type: Some("string".to_string()),
            max_size: Some(Bound::Limit(10)),
            one_of: Some(OneOfList(vec![vec!["a".to_string()]])),
            ..Default::default()
        };
        assert!(
            string_with_one_of
                .validate("str")
                .unwrap_err()
                .to_string()
                .contains("cannot specify 'one_of'")
        );

        let ref_with_required = PropertyDef {
            reference: Some("#/Ref".to_string()),
            required: Some(vec!["a".to_string()]),
            ..Default::default()
        };
        assert!(
            ref_with_required
                .validate("ref_prop")
                .unwrap_err()
                .to_string()
                .contains("cannot specify 'required'")
        );
    }
}

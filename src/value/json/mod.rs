use std::{
    cmp::Ordering,
    collections::{
        btree_map::Entry,
        BTreeMap,
        BTreeSet,
    },
    num::FpCategory,
};

use anyhow::Context;
use serde_json::{
    json,
    Value as JsonValue,
};

use crate::value::{
    DocumentId,
    Value,
};

mod bytes;
mod float;
mod integer;

/// Is a floating point number native zero?
fn is_negative_zero(n: f64) -> bool {
    matches!(n.total_cmp(&-0.0), Ordering::Equal)
}

impl From<Value> for JsonValue {
    fn from(value: Value) -> JsonValue {
        match value {
            Value::Id(id) => json!({ "$id": id.0 }),
            Value::Null => JsonValue::Null,
            Value::Int64(n) => json!({ "$integer": integer::JsonInteger::encode(n) }),
            Value::Float64(n) => {
                let mut is_special = is_negative_zero(n);
                is_special |= match n.classify() {
                    FpCategory::Zero | FpCategory::Normal | FpCategory::Subnormal => false,
                    FpCategory::Infinite | FpCategory::Nan => true,
                };
                if is_special {
                    json!({ "$float": float::JsonFloat::encode(n) })
                } else {
                    json!(n)
                }
            },
            Value::Boolean(b) => json!(b),
            Value::String(s) => json!(s),
            Value::Bytes(b) => json!({ "$bytes": bytes::JsonBytes::encode(&b) }),
            Value::Array(a) => JsonValue::from(a),
            Value::Set(s) => {
                let items: Vec<_> = s.into_iter().map(JsonValue::from).collect();
                json!({
                    "$set": items,
                })
            },
            Value::Map(m) => {
                let items: Vec<_> = m
                    .into_iter()
                    .map(|(k, v)| [JsonValue::from(k), JsonValue::from(v)])
                    .collect();
                json!({
                    "$map": items,
                })
            },
            Value::Object(o) => o.into_iter().collect(),
        }
    }
}

impl TryFrom<JsonValue> for Value {
    type Error = anyhow::Error;

    fn try_from(value: JsonValue) -> anyhow::Result<Self> {
        let r = match value {
            JsonValue::Null => Self::Null,
            JsonValue::Bool(b) => Self::from(b),
            JsonValue::Number(n) => {
                // TODO: JSON supports arbitrary precision numbers?
                let n = n
                    .as_f64()
                    .context("Arbitrary precision JSON integers unsupported")?;
                Value::from(n)
            },
            JsonValue::String(s) => Self::try_from(s)?,
            JsonValue::Array(arr) => {
                let mut out = Vec::with_capacity(arr.len());
                for a in arr {
                    out.push(Value::try_from(a)?);
                }
                Value::Array(out)
            },
            JsonValue::Object(map) => {
                if map.len() == 1 {
                    let (key, value) = map.into_iter().next().unwrap();
                    match &key[..] {
                        "$id" => {
                            let s: String = serde_json::from_value(value)?;
                            Self::Id(DocumentId(s))
                        },
                        "$bytes" => {
                            let i: String = serde_json::from_value(value)?;
                            Self::Bytes(bytes::JsonBytes::decode(i)?)
                        },
                        "$integer" => {
                            let i: String = serde_json::from_value(value)?;
                            Self::from(integer::JsonInteger::decode(i)?)
                        },
                        "$float" => {
                            let i: String = serde_json::from_value(value)?;
                            let n = float::JsonFloat::decode(i)?;
                            // Float64s encoded as a $float object must not fit into a regular
                            // `number`.
                            if !is_negative_zero(n) {
                                if let FpCategory::Normal | FpCategory::Subnormal = n.classify() {
                                    anyhow::bail!("Float64 {} should be encoded as a number", n);
                                }
                            }
                            Self::from(n)
                        },
                        "$set" => {
                            let items = match value {
                                JsonValue::Array(items) => items,
                                _ => anyhow::bail!("$set must have an array value"),
                            };
                            let mut set: BTreeSet<Value> = BTreeSet::new();
                            for item in items {
                                if let Some(old_value) = set.replace(Self::try_from(item)?) {
                                    anyhow::bail!("Duplicate value {old_value:?} in set");
                                }
                            }
                            Self::Set(set)
                        },
                        "$map" => {
                            let entries: Vec<[JsonValue; 2]> = serde_json::from_value(value)?;
                            let mut out = BTreeMap::new();
                            for [k, v] in entries {
                                match out.entry(Value::try_from(k)?) {
                                    Entry::Vacant(e) => {
                                        e.insert(Value::try_from(v)?);
                                    },
                                    Entry::Occupied(e) => {
                                        anyhow::bail!("Duplicate key {:?} in map", e.key())
                                    },
                                }
                            }
                            Self::Map(out)
                        },
                        _ => {
                            let mut fields = BTreeMap::new();
                            fields.insert(key, Self::try_from(value)?);
                            Self::Object(fields)
                        },
                    }
                } else {
                    let mut fields = BTreeMap::new();
                    for (key, value) in map {
                        fields.insert(key, Self::try_from(value)?);
                    }
                    Self::Object(fields)
                }
            },
        };
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use convex_sync_types::testing::assert_roundtrips;
    use proptest::prelude::*;
    use serde_json::Value as JsonValue;

    use crate::Value;

    proptest! {
        #![proptest_config(ProptestConfig { failure_persistence: None, .. ProptestConfig::default() })]

        #[test]
        fn test_value_roundtrips(value in any::<Value>()) {
            assert_roundtrips::<Value, JsonValue>(value);
        }
    }

    #[test]
    fn test_value_roundtrips_trophies() {
        let trophies = vec![
            Value::Float64(1.0),
            Value::Float64(f64::NAN),
            Value::Array(vec![Value::Float64(f64::NAN)]),
        ];
        for trophy in trophies {
            assert_roundtrips::<Value, JsonValue>(trophy);
        }
    }
}

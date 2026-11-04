//! Canonical, duplicate-aware RPC decoding for SDK input and output.
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::fmt;
struct UniqueValue(Value);
const DUPLICATE_MEMBER_ERROR: &str = "__thegn_duplicate_json_member__";

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct UniqueValueVisitor;

        impl<'de> Visitor<'de> for UniqueValueVisitor {
            type Value = UniqueValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON value without duplicate object members")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Bool(value)))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Number(value.into())))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Number(value.into())))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let number = serde_json::Number::from_f64(value)
                    .ok_or_else(|| E::custom("invalid JSON number"))?;
                Ok(UniqueValue(Value::Number(number)))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::String(value.to_owned())))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::String(value)))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(UniqueValue(value)) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(UniqueValue(Value::Array(values)))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(de::Error::custom(DUPLICATE_MEMBER_ERROR));
                    }
                    let UniqueValue(value) = map.next_value()?;
                    values.insert(key, value);
                }
                Ok(UniqueValue(Value::Object(values)))
            }
        }

        deserializer.deserialize_any(UniqueValueVisitor)
    }
}

pub(crate) fn decode(text: &str) -> Result<Value, ()> {
    if json_depth_exceeds(text, 64) {
        return Err(());
    }
    let UniqueValue(value) = serde_json::from_str(text).map_err(|_| ())?;
    let object = value.as_object().ok_or(())?;
    if object.get("method").is_some() {
        if object
            .keys()
            .any(|k| !matches!(k.as_str(), "method" | "params" | "id"))
        {
            return Err(());
        }
        let method = object["method"].as_str().ok_or(())?;
        if method.is_empty() || method.len() > 128 || method.chars().any(char::is_control) {
            return Err(());
        }
        if object.get("id").is_some_and(|id| !id.is_u64()) {
            return Err(());
        }
    } else {
        if object
            .keys()
            .any(|k| !matches!(k.as_str(), "result" | "error" | "id"))
            || object.contains_key("result") == object.contains_key("error")
            || !object.get("id").is_some_and(Value::is_u64)
        {
            return Err(());
        }
        if let Some(error) = object.get("error") {
            let error = error.as_object().ok_or(())?;
            if error
                .keys()
                .any(|k| !matches!(k.as_str(), "code" | "message" | "data"))
                || error.get("message").and_then(Value::as_str).is_none()
                || serde_json::from_value::<thegn_core::plugin_api::RpcErrorCode>(
                    error.get("code").cloned().ok_or(())?,
                )
                .is_err()
            {
                return Err(());
            }
        }
    }
    Ok(value)
}
fn json_depth_exceeds(text: &str, limit: usize) -> bool {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for byte in text.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }

        match byte {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth = depth.saturating_add(1);
                if depth > limit {
                    return true;
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }

    false
}

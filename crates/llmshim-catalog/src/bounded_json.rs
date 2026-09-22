use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use std::fmt;
use std::mem::size_of;

const COMPLEXITY_MARKER: &str = "llmshim-json-complexity-limit";

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_depth: usize,
    pub max_nodes: usize,
    pub max_owned_bytes: usize,
}

impl Limits {
    pub const UNARY: Self = Self {
        max_depth: 128,
        max_nodes: 65_536,
        max_owned_bytes: 40 * 1024 * 1024,
    };
    pub const SSE: Self = Self {
        max_depth: 128,
        max_nodes: 16_384,
        max_owned_bytes: 16 * 1024 * 1024,
    };
    pub const INBOUND: Self = Self {
        max_depth: 128,
        max_nodes: 32_768,
        max_owned_bytes: 8 * 1024 * 1024,
    };
    pub const OAUTH: Self = Self {
        max_depth: 64,
        max_nodes: 4_096,
        max_owned_bytes: 1024 * 1024,
    };
    pub const CATALOG: Self = Self {
        max_depth: 128,
        max_nodes: 262_144,
        max_owned_bytes: 128 * 1024 * 1024,
    };
}

#[derive(Debug)]
pub enum ParseError {
    Malformed(serde_json::Error),
    Complexity,
}

pub fn parse_slice(input: &[u8], limits: Limits) -> Result<Value, ParseError> {
    let mut deserializer = serde_json::Deserializer::from_slice(input);
    parse(&mut deserializer, limits)
}

pub fn parse_str(input: &str, limits: Limits) -> Result<Value, ParseError> {
    let mut deserializer = serde_json::Deserializer::from_str(input);
    parse(&mut deserializer, limits)
}

fn parse<'de, R>(
    deserializer: &mut serde_json::Deserializer<R>,
    limits: Limits,
) -> Result<Value, ParseError>
where
    R: serde_json::de::Read<'de>,
{
    let mut budget = Budget {
        limits,
        nodes: 0,
        owned_bytes: 0,
    };
    let result = ValueSeed {
        budget: &mut budget,
        depth: 1,
    }
    .deserialize(&mut *deserializer)
    .and_then(|value| {
        deserializer.end()?;
        Ok(value)
    });
    result.map_err(|error| {
        if error.to_string().contains(COMPLEXITY_MARKER) {
            ParseError::Complexity
        } else {
            ParseError::Malformed(error)
        }
    })
}

struct Budget {
    limits: Limits,
    nodes: usize,
    owned_bytes: usize,
}

impl Budget {
    fn charge<E: serde::de::Error>(&mut self, bytes: usize) -> Result<(), E> {
        self.nodes = self.nodes.checked_add(1).ok_or_else(complexity::<E>)?;
        self.owned_bytes = self
            .owned_bytes
            .checked_add(size_of::<Value>())
            .and_then(|value| value.checked_add(bytes))
            .ok_or_else(complexity::<E>)?;
        if self.nodes > self.limits.max_nodes || self.owned_bytes > self.limits.max_owned_bytes {
            return Err(complexity());
        }
        Ok(())
    }

    fn charge_key<E: serde::de::Error>(&mut self, bytes: usize) -> Result<(), E> {
        self.charge_bytes(bytes)
    }

    fn charge_bytes<E: serde::de::Error>(&mut self, bytes: usize) -> Result<(), E> {
        self.owned_bytes = self
            .owned_bytes
            .checked_add(bytes)
            .ok_or_else(complexity::<E>)?;
        if self.owned_bytes > self.limits.max_owned_bytes {
            return Err(complexity());
        }
        Ok(())
    }
}

fn complexity<E: serde::de::Error>() -> E {
    E::custom(COMPLEXITY_MARKER)
}

struct ValueSeed<'a> {
    budget: &'a mut Budget,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for ValueSeed<'_> {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if self.depth > self.budget.limits.max_depth {
            return Err(complexity());
        }
        deserializer.deserialize_any(ValueVisitor {
            budget: self.budget,
            depth: self.depth,
        })
    }
}

struct ValueVisitor<'a> {
    budget: &'a mut Budget,
    depth: usize,
}

impl<'de> Visitor<'de> for ValueVisitor<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded JSON value")
    }

    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Value, E> {
        self.budget.charge(0)?;
        Ok(Value::Bool(value))
    }
    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Value, E> {
        self.budget.charge(0)?;
        Ok(Value::Number(value.into()))
    }
    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Value, E> {
        self.budget.charge(0)?;
        Ok(Value::Number(value.into()))
    }
    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Value, E> {
        self.budget.charge(0)?;
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("invalid JSON number"))
    }
    fn visit_none<E: serde::de::Error>(self) -> Result<Value, E> {
        self.visit_unit()
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        self.budget.charge(0)?;
        Ok(Value::Null)
    }
    fn visit_borrowed_str<E: serde::de::Error>(self, value: &'de str) -> Result<Value, E> {
        let mut owned = String::new();
        owned
            .try_reserve_exact(value.len())
            .map_err(|_| complexity())?;
        self.budget.charge(owned.capacity())?;
        owned.push_str(value);
        Ok(Value::String(owned))
    }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Value, E> {
        let mut owned = String::new();
        owned
            .try_reserve_exact(value.len())
            .map_err(|_| complexity())?;
        self.budget.charge(owned.capacity())?;
        owned.push_str(value);
        Ok(Value::String(owned))
    }
    fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Value, E> {
        self.budget.charge(value.capacity())?;
        Ok(Value::String(value))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
        self.budget.charge(0)?;
        let mut values = Vec::new();
        loop {
            let old_capacity = values.capacity();
            values.try_reserve(1).map_err(|_| complexity())?;
            let capacity_bytes = values
                .capacity()
                .checked_sub(old_capacity)
                .and_then(|slots| slots.checked_mul(size_of::<Value>()))
                .ok_or_else(complexity::<A::Error>)?;
            self.budget.charge_bytes(capacity_bytes)?;
            match sequence.next_element_seed(ValueSeed {
                budget: self.budget,
                depth: self.depth + 1,
            })? {
                Some(value) => values.push(value),
                None => break,
            }
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        self.budget.charge(0)?;
        let mut values = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let entry_bytes = key
                .capacity()
                .checked_add(size_of::<String>() + size_of::<Value>() + 3 * size_of::<usize>())
                .ok_or_else(complexity::<A::Error>)?;
            self.budget.charge_key(entry_bytes)?;
            let value = map.next_value_seed(ValueSeed {
                budget: self.budget,
                depth: self.depth + 1,
            })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_node_and_owned_byte_thresholds() {
        let limits = Limits {
            max_depth: 8,
            max_nodes: 3,
            max_owned_bytes: 224,
        };
        assert!(parse_str("[0,1]", limits).is_ok());
        assert!(matches!(
            parse_str("[0,1,2]", limits),
            Err(ParseError::Complexity)
        ));
        let string_limits = Limits {
            max_depth: 8,
            max_nodes: 1,
            max_owned_bytes: 36,
        };
        assert!(parse_str("\"abcd\"", string_limits).is_ok());
        assert!(matches!(
            parse_str("\"abcde\"", string_limits),
            Err(ParseError::Complexity)
        ));
    }

    #[test]
    fn depth_duplicate_keys_and_malformed_json_are_explicit() {
        assert!(parse_str(
            "[[0]]",
            Limits {
                max_depth: 3,
                max_nodes: 8,
                max_owned_bytes: 1024
            }
        )
        .is_ok());
        assert!(matches!(
            parse_str(
                "[[[0]]]",
                Limits {
                    max_depth: 3,
                    max_nodes: 8,
                    max_owned_bytes: 1024
                }
            ),
            Err(ParseError::Complexity)
        ));
        assert_eq!(parse_str("{\"x\":1,\"x\":2}", Limits::SSE).unwrap()["x"], 2);
        assert!(matches!(
            parse_str("{", Limits::UNARY),
            Err(ParseError::Malformed(_))
        ));
    }

    #[test]
    fn production_profiles_accept_large_text_and_reject_compact_width() {
        let large_text = format!("\"{}\"", "x".repeat(1024 * 1024));
        assert!(parse_str(&large_text, Limits::UNARY).is_ok());
        let wide = format!(
            "[{}]",
            std::iter::repeat_n("0", Limits::SSE.max_nodes)
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(matches!(
            parse_str(&wide, Limits::SSE),
            Err(ParseError::Complexity)
        ));
    }

    #[test]
    fn object_entries_and_underfilled_nested_array_capacity_are_charged() {
        let object = r#"{"a":0,"b":1}"#;
        assert!(matches!(
            parse_str(
                object,
                Limits {
                    max_depth: 8,
                    max_nodes: 8,
                    max_owned_bytes: 200,
                }
            ),
            Err(ParseError::Complexity)
        ));
        let nested = "[[0],[1]]";
        assert!(matches!(
            parse_str(
                nested,
                Limits {
                    max_depth: 8,
                    max_nodes: 8,
                    max_owned_bytes: 350,
                }
            ),
            Err(ParseError::Complexity)
        ));
    }
}

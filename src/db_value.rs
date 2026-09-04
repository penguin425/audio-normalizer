//! Lossless JSON representation for decibel-domain measurements.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A decibel-domain value whose three observable states survive JSON.
///
/// JSON encoding is a finite number, the string `"-inf"` for measured
/// digital silence, or `null` when the measurement is undefined/not run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DecibelValue {
    Finite(f64),
    NegativeInfinity,
    Undefined,
}

impl DecibelValue {
    pub fn from_db(value: f64) -> Self {
        if value.is_finite() {
            Self::Finite(value)
        } else if value == f64::NEG_INFINITY {
            Self::NegativeInfinity
        } else {
            Self::Undefined
        }
    }

    pub fn from_optional_db(value: Option<f64>) -> Self {
        value.map_or(Self::Undefined, Self::from_db)
    }

    pub const fn as_db(self) -> Option<f64> {
        match self {
            Self::Finite(value) => Some(value),
            Self::NegativeInfinity => Some(f64::NEG_INFINITY),
            Self::Undefined => None,
        }
    }
}

impl Serialize for DecibelValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match *self {
            Self::Finite(value) => serializer.serialize_f64(value),
            Self::NegativeInfinity => serializer.serialize_str("-inf"),
            Self::Undefined => serializer.serialize_none(),
        }
    }
}

impl<'de> Deserialize<'de> for DecibelValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum WireValue {
            Finite(f64),
            Symbol(String),
            Undefined(()),
        }

        match WireValue::deserialize(deserializer)? {
            WireValue::Finite(value) if value.is_finite() => Ok(Self::Finite(value)),
            WireValue::Finite(_) => Err(serde::de::Error::custom("decibel number must be finite")),
            WireValue::Symbol(value) if value == "-inf" => Ok(Self::NegativeInfinity),
            WireValue::Symbol(_) => Err(serde::de::Error::custom(
                "unsupported symbolic decibel value",
            )),
            WireValue::Undefined(()) => Ok(Self::Undefined),
        }
    }
}

pub(crate) fn serialize_db<S>(value: &f64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    DecibelValue::from_db(*value).serialize(serializer)
}

pub(crate) fn serialize_optional_db<S>(
    value: &Option<f64>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    DecibelValue::from_optional_db(*value).serialize(serializer)
}

pub(crate) fn deserialize_db<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(DecibelValue::deserialize(deserializer)?
        .as_db()
        .unwrap_or(f64::NAN))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_distinguishes_all_three_states() {
        assert_eq!(
            serde_json::to_string(&DecibelValue::Finite(-23.0)).unwrap(),
            "-23.0"
        );
        assert_eq!(
            serde_json::to_string(&DecibelValue::NegativeInfinity).unwrap(),
            "\"-inf\""
        );
        assert_eq!(
            serde_json::to_string(&DecibelValue::Undefined).unwrap(),
            "null"
        );
        assert_eq!(
            serde_json::from_str::<DecibelValue>("\"-inf\"").unwrap(),
            DecibelValue::NegativeInfinity
        );
    }
}

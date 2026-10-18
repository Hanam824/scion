use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// BLAKE3 hash identifying an immutable object (blob, tree or changeset).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub [u8; 32]);

#[derive(Debug, thiserror::Error)]
#[error("invalid object id: {0:?}")]
pub struct InvalidId(pub String);

impl ObjectId {
    pub fn hash(data: &[u8]) -> Self {
        ObjectId(*blake3::hash(data).as_bytes())
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn short(&self) -> String {
        self.to_hex()[..12].to_string()
    }

    pub fn from_hex(s: &str) -> Result<Self, InvalidId> {
        let bytes = hex::decode(s).map_err(|_| InvalidId(s.to_string()))?;
        let arr: [u8; 32] = bytes.try_into().map_err(|_| InvalidId(s.to_string()))?;
        Ok(ObjectId(arr))
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", self.short())
    }
}

impl FromStr for ObjectId {
    type Err = InvalidId;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

impl Serialize for ObjectId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ObjectId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        ObjectId::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let id = ObjectId::hash(b"hello");
        assert_eq!(ObjectId::from_hex(&id.to_hex()).unwrap(), id);
        assert!(ObjectId::from_hex("abc").is_err());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<ObjectId>(&json).unwrap(), id);
    }
}

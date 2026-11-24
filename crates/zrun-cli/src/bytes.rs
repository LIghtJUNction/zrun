use serde::de::Error;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;

#[derive(Serialize, Deserialize)]
#[serde(tag = "encoding", content = "data", rename_all = "snake_case")]
enum Bytes<'a> {
    Utf8(Cow<'a, str>),
    Base64(Cow<'a, str>),
}

pub(crate) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    if let Ok(text) = std::str::from_utf8(bytes)
        && !text
            .bytes()
            .any(|byte| byte.is_ascii_control() && !matches!(byte, b'\n' | b'\r' | b'\t'))
    {
        return Bytes::Utf8(Cow::Borrowed(text)).serialize(serializer);
    }
    let size = z64::encoded_len(bytes.len(), z64::Mode::Standard)
        .ok_or_else(|| serde::ser::Error::custom("base64 output size overflow"))?;
    let mut out = vec![0; size];
    z64::encode(bytes, &mut out, z64::Mode::Standard).map_err(serde::ser::Error::custom)?;
    let text = String::from_utf8(out).map_err(serde::ser::Error::custom)?;
    Bytes::Base64(Cow::Owned(text)).serialize(serializer)
}

pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    match Bytes::deserialize(deserializer)? {
        Bytes::Utf8(text) => Ok(text.into_owned().into_bytes()),
        Bytes::Base64(text) => {
            let size = z64::decoded_capacity(text.len())
                .ok_or_else(|| Error::custom("base64 input size overflow"))?;
            let mut out = vec![0; size];
            let used = z64::decode(text.as_bytes(), &mut out, z64::Mode::Standard)
                .map_err(Error::custom)?;
            out.truncate(used);
            Ok(out)
        }
    }
}

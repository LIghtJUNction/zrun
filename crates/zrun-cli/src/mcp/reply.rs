use std::sync::Arc;

use serde_json::Value;

use crate::execution::{Delivery, Disposition, EncodedResult};

pub(super) struct Reply {
    parts: Vec<Part>,
}

enum Part {
    Literal(&'static [u8]),
    Owned(Vec<u8>),
    Result(Arc<EncodedResult>, Representation),
}

#[derive(Clone, Copy)]
enum Representation {
    Json,
    JsonString,
}

impl Reply {
    pub(super) fn json(value: &Value) -> serde_json::Result<Self> {
        Ok(Self {
            parts: vec![Part::Owned(serde_json::to_vec(value)?)],
        })
    }

    pub(super) fn tool(id: &Value, records: &[Delivery], batch: bool) -> serde_json::Result<Self> {
        let failed = records
            .iter()
            .any(|delivery| delivery.result.exit_code != 0);
        let records = records
            .iter()
            .map(|delivery| Ok((delivery.disposition, delivery.result.encode()?)))
            .collect::<serde_json::Result<Vec<_>>>()?;
        let mut reply = Self {
            parts: vec![
                Part::Literal(br#"{"jsonrpc":"2.0","id":"#),
                Part::Owned(serde_json::to_vec(id)?),
                Part::Literal(b",\"result\":{\"content\":[{\"type\":\"text\",\"text\":\""),
            ],
        };
        reply.append_records(&records, batch, Representation::JsonString);
        reply.literal(b"\"}],\"structuredContent\":");
        reply.append_records(&records, batch, Representation::Json);
        reply.literal(if failed {
            br#","isError":true}}"#
        } else {
            br#","isError":false}}"#
        });
        Ok(reply)
    }

    pub(super) fn len(&self) -> usize {
        self.chunks().map(<[u8]>::len).sum()
    }

    pub(super) fn chunks(&self) -> impl Iterator<Item = &[u8]> {
        self.parts.iter().map(|part| match part {
            Part::Literal(bytes) => bytes,
            Part::Owned(bytes) => bytes.as_slice(),
            Part::Result(result, Representation::Json) => result.json(),
            Part::Result(result, Representation::JsonString) => result.json_string_contents(),
        })
    }

    fn literal(&mut self, bytes: &'static [u8]) {
        self.parts.push(Part::Literal(bytes));
    }

    fn append_records(
        &mut self,
        records: &[(Disposition, Arc<EncodedResult>)],
        batch: bool,
        representation: Representation,
    ) {
        let (commands, disposition, result) = match representation {
            Representation::Json => (
                br#"{"commands":["#.as_slice(),
                br#"{"disposition":""#.as_slice(),
                br#"","result":"#.as_slice(),
            ),
            Representation::JsonString => (
                br#"{\"commands\":["#.as_slice(),
                br#"{\"disposition\":\""#.as_slice(),
                br#"\",\"result\":"#.as_slice(),
            ),
        };
        if batch {
            self.literal(commands);
        }
        for (index, (kind, encoded)) in records.iter().enumerate() {
            if index != 0 {
                self.literal(b",");
            }
            self.literal(disposition);
            self.literal(match kind {
                Disposition::Executed => b"executed",
                Disposition::Coalesced => b"coalesced",
                Disposition::Cached => b"cached",
                Disposition::Rejected => b"rejected",
            });
            self.literal(result);
            self.parts
                .push(Part::Result(encoded.clone(), representation));
            self.literal(b"}");
        }
        if batch {
            self.literal(b"]}");
        }
    }
}

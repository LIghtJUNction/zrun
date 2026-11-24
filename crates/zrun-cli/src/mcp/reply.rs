use std::sync::Arc;

use serde_json::Value;

use crate::execution::{Delivery, Disposition, EncodedResult, EncodedSize};
use crate::output::Budget;

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

    pub(super) async fn tool(
        id: &Value,
        records: &[Delivery],
        batch: bool,
        budget: &Arc<Budget>,
    ) -> serde_json::Result<Self> {
        let failed = records
            .iter()
            .any(|delivery| delivery.result.exit_code != 0);
        let records = encode_records(records, budget).await?;
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

enum Encoding {
    Shared(Arc<EncodedResult>),
    Required(EncodedSize),
}

async fn encode_records(
    records: &[Delivery],
    budget: &Arc<Budget>,
) -> serde_json::Result<Vec<(Disposition, Arc<EncodedResult>)>> {
    let mut selected = Vec::with_capacity(records.len());
    loop {
        let generation = budget.generation();
        let mut required = 0;
        for delivery in records {
            let encoding = if let Some(encoded) = delivery.result.prepared_encoding() {
                Encoding::Shared(encoded)
            } else {
                let size = delivery.result.encoded_size()?;
                required += size.total();
                Encoding::Required(size)
            };
            selected.push(encoding);
        }

        if let Some(mut reservation) = budget.try_reserve(required) {
            return records
                .iter()
                .zip(selected)
                .map(|(delivery, encoding)| {
                    let encoded = match encoding {
                        Encoding::Shared(encoded) => encoded,
                        Encoding::Required(size) => delivery
                            .result
                            .encode(size, reservation.split(size.total()))?,
                    };
                    Ok((delivery.disposition, encoded))
                })
                .collect();
        }
        // A blocked batch never retains an encoded fragment. All of its
        // missing bytes will be reserved together on a later attempt.
        selected.clear();
        budget.changed(generation).await;
    }
}

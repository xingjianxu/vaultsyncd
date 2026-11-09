//! Bounded SSE framing. Hints can only wake the durable changes receiver, never carry changes.
use crate::ClientError;

#[derive(Default)]
pub(crate) struct Decoder {
    bytes: Vec<u8>,
}
impl Decoder {
    pub(crate) fn push(&mut self, chunk: &[u8], vault: &str) -> Result<usize, ClientError> {
        let mut hints = 0;
        // Parse incrementally even if a transport chunk contains many small frames. Reject a
        // single oversized frame before allocating more than the bounded 64KiB buffer.
        for byte in chunk {
            if self.bytes.len() >= 65536 {
                return Err(ClientError::invalid_request("SSE frame too large"));
            }
            self.bytes.push(*byte);
            if self.bytes.ends_with(b"\n\n") || self.bytes.ends_with(b"\r\n\r\n") {
                let text = std::str::from_utf8(&self.bytes)
                    .map_err(|_| ClientError::invalid_request("SSE UTF-8"))?;
                let mut event = None;
                let mut data = Vec::new();
                for line in text.lines() {
                    if let Some(value) = line.strip_prefix("event:") {
                        event = Some(value.trim());
                    }
                    if let Some(value) = line.strip_prefix("data:") {
                        data.push(value.trim_start());
                    }
                }
                if event == Some("seq") {
                    let hint: vaultsyncd_protocol::generated::SeqHint =
                        serde_json::from_str(&data.join("\n"))
                            .map_err(|_| ClientError::invalid_request("malformed SSE hint"))?;
                    if hint.vault_id != vault
                        || vaultsyncd_protocol::decimal::validate(&hint.seq).is_err()
                    {
                        return Err(ClientError::invalid_request("SSE vault/seq mismatch"));
                    }
                    hints += 1;
                }
                self.bytes.clear();
            }
        }
        Ok(hints)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    const VAULT: &str = "00112233445566778899aabbccddeeff";
    fn frame(seq: &str) -> Vec<u8> {
        format!(
            "event: seq\r\nid: {seq}\r\ndata: {{\"vault_id\":\"{VAULT}\",\"seq\":\"{seq}\"}}\r\n\r\n"
        )
        .into_bytes()
    }
    #[test]
    fn split_frames_comments_duplicates_and_reordering_only_wake() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.push(b": keepalive\n\n", VAULT).unwrap(), 0);
        for seq in ["4", "4", "9", "3"] {
            let frame = frame(seq);
            let mut count = 0;
            for part in frame.chunks(3) {
                count += decoder.push(part, VAULT).unwrap();
            }
            assert_eq!(count, 1);
        }
    }
    #[test]
    fn malformed_foreign_and_oversized_frames_are_rejected() {
        assert!(Decoder::default().push(&frame("01"), VAULT).is_err());
        assert!(Decoder::default().push(&frame("1"), "different").is_err());
        assert!(Decoder::default().push(&vec![b'x'; 65537], VAULT).is_err());
    }
}

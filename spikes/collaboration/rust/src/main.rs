// Compatibility probe only. This is not MockPad's HTTP/WebSocket server.
use serde::Deserialize;
use std::io::{self, BufRead, Write};
use yrs::updates::{decoder::Decode, encoder::Encode};
use yrs::{Doc, GetString, ReadTxn, StateVector, Transact, Update};

#[derive(Deserialize)]
struct Request {
    update: Vec<u8>,
    vector: Vec<u8>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let doc = Doc::new();
    let text = doc.get_or_insert_text("code");
    let mut stdout = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let request: Request = serde_json::from_str(&line?)?;
        let update = Update::decode_v1(&request.update)?;
        doc.transact_mut().apply_update(update)?;
        let vector = StateVector::decode_v1(&request.vector)?;
        let txn = doc.transact();
        let response = serde_json::json!({
            "update": txn.encode_state_as_update_v1(&vector),
            "vector": txn.state_vector().encode_v1(),
            "text": text.get_string(&txn),
        });
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}

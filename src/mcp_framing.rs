use anyhow::{Result, bail};
use serde_json::Value;
use std::io::{BufRead, Write};

pub(crate) fn read_message<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Ok(None);
        }

        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }

        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            let len = value.trim().parse::<usize>()?;
            loop {
                line.clear();
                let bytes = reader.read_line(&mut line)?;
                if bytes == 0 {
                    bail!("unexpected EOF before MCP message body");
                }
                if line.trim_end_matches(['\r', '\n']).is_empty() {
                    break;
                }
            }
            let mut body = vec![0; len];
            reader.read_exact(&mut body)?;
            return Ok(Some(body));
        }

        return Ok(Some(trimmed.as_bytes().to_vec()));
    }
}

pub(crate) fn write_json<W: Write>(writer: &mut W, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

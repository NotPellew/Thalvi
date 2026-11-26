//! The wire form of the request/reply channel: one JSON line each way, read within a size bound.

use std::io::{BufReader, Read};

use serde_json::json;

/// The longest request the running app reads.
pub const MAX_REQUEST: usize = 256 * 1024;

/// The longest reply a client reads.
pub const MAX_REPLY: usize = 1024 * 1024;

/// Why a line could not be read.
#[derive(Debug, PartialEq, Eq)]
pub enum LineError {
    TooLong,
    NotText,
    Io(std::io::ErrorKind),
}

/// Reads up to the first newline, or to the end of the stream; `max` bytes at most.
///
/// `None` is a stream that ended before any byte, which is a connection that was opened and closed
/// rather than a line; an empty line still reads as `Some("")`.
///
/// # Errors
/// [`LineError::TooLong`] when more than `max` bytes come before a newline, [`LineError::NotText`]
/// when they are not UTF-8, and [`LineError::Io`] when reading fails or times out.
pub fn read_line(reader: impl Read, max: usize) -> Result<Option<String>, LineError> {
    let mut line = Vec::new();
    let mut any = false;
    for byte in BufReader::new(reader.take(max as u64 + 2)).bytes() {
        let byte = byte.map_err(|error| LineError::Io(error.kind()))?;
        any = true;
        if byte == b'\n' {
            break;
        }
        line.push(byte);
    }
    if !any {
        return Ok(None);
    }
    if line.len() > max {
        return Err(LineError::TooLong);
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|_| LineError::NotText)
}

/// The reply to a request that never reached a command.
#[must_use]
pub fn refusal(message: &str) -> String {
    json!({"ok": false, "error": message}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_read_up_to_its_newline_and_a_size_bound() {
        let max = 8;
        let cases: [(&[u8], _); 8] = [
            (b"status\n", Ok(Some("status".to_owned()))),
            (b"status", Ok(Some("status".to_owned()))),
            (b"two\nlines\n", Ok(Some("two".to_owned()))),
            (b"12345678\n", Ok(Some("12345678".to_owned()))),
            (b"123456789\n", Err(LineError::TooLong)),
            (b"\xff\n", Err(LineError::NotText)),
            (b"\n", Ok(Some(String::new()))),
            (b"", Ok(None)),
        ];
        for (input, expected) in cases {
            assert_eq!(read_line(input, max), expected, "{input:?}");
        }
    }
}

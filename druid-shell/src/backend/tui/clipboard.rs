// Copyright 2026 the Druid Authors
// SPDX-License-Identifier: Apache-2.0

//! Terminal clipboard support.

use std::io::{self, Write};

use crate::clipboard::{ClipboardFormat, FormatId};

#[derive(Debug, Clone, Default)]
pub struct Clipboard;

impl Clipboard {
    pub fn put_string(&mut self, string: impl AsRef<str>) {
        write_osc52(string.as_ref().as_bytes());
    }

    pub fn put_formats(&mut self, formats: &[ClipboardFormat]) {
        if let Some(format) = formats
            .iter()
            .find(|format| format.identifier == ClipboardFormat::TEXT)
        {
            write_osc52(&format.data);
        }
    }

    pub fn get_string(&self) -> Option<String> {
        None
    }

    pub fn preferred_format(&self, _formats: &[FormatId]) -> Option<FormatId> {
        None
    }

    pub fn get_format(&self, _format: FormatId) -> Option<Vec<u8>> {
        None
    }

    pub fn available_type_names(&self) -> Vec<String> {
        Vec::new()
    }
}

fn write_osc52(data: &[u8]) {
    let mut stdout = io::stdout().lock();
    let _ = write!(stdout, "\x1b]52;c;{}\x07", base64(data));
    let _ = stdout.flush();
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(data.len().div_ceil(3) * 4);

    for chunk in data.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);

        encoded.push(TABLE[(first >> 2) as usize] as char);
        encoded.push(TABLE[(((first & 0b11) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(TABLE[(((second & 0b1111) << 2) | (third >> 6)) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(TABLE[(third & 0b11_1111) as usize] as char);
        } else {
            encoded.push('=');
        }
    }

    encoded
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn encodes_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"hello world"), "aGVsbG8gd29ybGQ=");
    }
}

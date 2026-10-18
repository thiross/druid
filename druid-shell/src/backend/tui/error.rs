// Copyright 2026 the Druid Authors
// SPDX-License-Identifier: Apache-2.0

//! Terminal backend errors.

use std::fmt;

#[derive(Debug, Clone)]
pub enum Error {
    Io(String),
    Unsupported(&'static str),
}

impl Error {
    pub(crate) fn unsupported(message: &'static str) -> Self {
        Error::Unsupported(message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(message) => write!(f, "terminal I/O error: {message}"),
            Error::Unsupported(message) => write!(f, "unsupported by the TUI backend: {message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Io(error.to_string())
    }
}

//! Load errors.
//!
//! These exist so a consumer can tell "the file is missing" from "the file is
//! there but no longer matches these structs". The second case is the one that
//! matters during a vectors regeneration: it means the generator and this crate
//! have to change together.

use std::path::PathBuf;

use crate::VectorFamily;

/// What went wrong while locating or reading a vector family.
#[derive(Debug)]
pub enum VectorError {
    /// The vectors directory was not found. `tried` lists every candidate path in
    /// the order they were attempted, so a failure is diagnosable without reading
    /// the loader.
    DirectoryNotFound {
        /// Paths that were tried, in order.
        tried: Vec<PathBuf>,
    },

    /// The vectors directory exists but the family's file is missing. Usually
    /// means the crate was built from a source tree without the committed data.
    FileMissing {
        /// The family that was requested.
        family: VectorFamily,
        /// The path that was checked.
        path: PathBuf,
    },

    /// The file exists but could not be read (permissions, I/O error).
    Read {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// The file is not the JSON shape this crate expects.
    ///
    /// The overwhelmingly common cause is that the generator changed a field name,
    /// added a field, or made a field nullable that used to be required. Every
    /// struct denies unknown fields on purpose, so this is a signal rather than
    /// noise.
    Decode {
        /// The family that failed to decode.
        family: VectorFamily,
        /// The path that was decoded.
        path: PathBuf,
        /// The underlying `serde_json` error, including the offset in the file.
        source: serde_json::Error,
    },
}

impl std::fmt::Display for VectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VectorError::DirectoryNotFound { tried } => {
                write!(
                    f,
                    "could not find the oracle vectors directory; tried {}",
                    tried
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            VectorError::FileMissing { family, path } => write!(
                f,
                "oracle vectors for {family} are missing at {}; they are committed data, \
                 so this usually means the tree is incomplete. If they were deleted, \
                 regenerate with `make vectors`.",
                path.display()
            ),
            VectorError::Read { path, source } => {
                write!(f, "cannot read {}: {source}", path.display())
            }
            VectorError::Decode {
                family,
                path,
                source,
            } => write!(
                f,
                "oracle vectors for {family} at {} no longer match the reader's structs: {source}\n\
                 The generator and `crates/dxdb-vectors` have to change together. Regenerate \
                 with `make vectors`; if a field was renamed or added, update the structs in \
                 this crate in the same commit.",
                path.display()
            ),
        }
    }
}

impl std::error::Error for VectorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VectorError::Read { source, .. } => Some(source),
            VectorError::Decode { source, .. } => Some(source),
            _ => None,
        }
    }
}

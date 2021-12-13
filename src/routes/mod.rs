pub mod core;
pub mod s3;

use std::ops::Deref;
use std::path::PathBuf;

use rocket::{request::FromSegments, http::uri::{fmt::Path, Segments, error::PathError}};

pub struct DotPathBuf(PathBuf);

impl<'r> FromSegments<'r> for DotPathBuf {
    type Error = PathError;
    fn from_segments(segments: Segments<'r, Path>) -> Result<Self, Self::Error> {
        segments.to_path_buf(true).map(DotPathBuf)
    }
}

impl Deref for DotPathBuf {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.0
    }
}

//! Audio2Face gRPC client.
//!
//! Streams PCM16 audio to an Audio2Face-compatible service and receives
//! blendshape coefficient frames. Includes a small jitter buffer so downstream
//! consumers (UE5 Face_AnimBP) see a steady frame cadence even when network
//! delivery is bursty.
//!
//! The generated proto is a placeholder that mirrors NVIDIA's public Audio2Face
//! streaming shape. Replace [proto/audio2face.proto] with the NVIDIA Inception
//! `.proto` when available.

// `tonic::Status` is 176 bytes, which trips `result_large_err` (a deny-level
// lint under CI's `-D warnings`) on every fallible gRPC call. Boxing our own
// `A2fStreamError` would silence two of the three sites and not the third:
// that one is inside the tonic-generated client, which we do not author and
// cannot annotate. The error type is upstream and the call sites are not hot,
// so the allow belongs at the crate root rather than scattered per function.
#![allow(clippy::result_large_err)]

pub mod client;
pub mod jitter;

// Generated gRPC types and client from proto/audio2face.proto (see build.rs).
pub mod proto {
    tonic::include_proto!("consistent_world.a2f.v1");
}

pub use client::{A2fClient, A2fStreamError, BlendshapeFrame, SessionConfig};
pub use jitter::JitterBuffer;

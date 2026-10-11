//! Audio capture, resampling, and Gemini Live WebSocket transport.
//!
//! The avatar pipeline's Layer 1 (Interpreter) requires a <200ms audio-to-audio
//! loop: mic capture at native rate -> 16kHz PCM16 mono -> WebSocket to Gemini
//! Live -> 24kHz PCM16 audio response -> playback.

pub mod capture;
pub mod gemini_live;
pub mod resample;

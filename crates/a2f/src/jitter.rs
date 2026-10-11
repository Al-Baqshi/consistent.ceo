//! Jitter buffer for blendshape frames.
//!
//! The Audio2Face stream arrives bursty; UE5's Face_AnimBP needs a steady
//! cadence. This buffer holds incoming frames and pops them on a fixed clock,
//! so consumers always see evenly-spaced frames with the target FPS.
//!
//! Policy:
//! - Hold up to `capacity` frames. If full, drop the oldest to keep latency bounded.
//! - `pop_due(now_ms)` returns frames whose `pts_ms` is <= the playback clock.
//! - No frame reordering: frames are assumed to arrive mostly in order.

use std::collections::VecDeque;

use crate::client::BlendshapeFrame;

/// Fixed-capacity ring of blendshape frames, popped on a playback clock (see
/// module docs above for the drop/cadence policy).
#[derive(Debug)]
pub struct JitterBuffer {
    frames: VecDeque<BlendshapeFrame>,
    capacity: usize,
    /// Offset subtracted from `pts_ms` when deciding "due": lets the caller
    /// delay playback by N ms to absorb jitter before the first pop.
    playout_delay_ms: u64,
}

impl JitterBuffer {
    pub fn new(capacity: usize, playout_delay_ms: u64) -> Self {
        Self {
            frames: VecDeque::with_capacity(capacity),
            capacity,
            playout_delay_ms,
        }
    }

    /// Adds a frame, dropping the oldest buffered frame if already at
    /// capacity.
    pub fn push(&mut self, frame: BlendshapeFrame) {
        if self.frames.len() >= self.capacity {
            self.frames.pop_front();
        }
        self.frames.push_back(frame);
    }

    /// Pops, in order, every buffered frame whose `pts_ms` is due relative to
    /// `clock_ms` once the playout delay is subtracted.
    pub fn pop_due(&mut self, clock_ms: u64) -> Vec<BlendshapeFrame> {
        let threshold = clock_ms.saturating_sub(self.playout_delay_ms);
        let mut out = Vec::new();
        while let Some(front) = self.frames.front() {
            if front.pts_ms <= threshold {
                out.push(self.frames.pop_front().unwrap());
            } else {
                break;
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Builds a minimal frame with just a timestamp, for test brevity.
    fn f(pts: u64) -> BlendshapeFrame {
        BlendshapeFrame {
            pts_ms: pts,
            weights: vec![0.0; 52],
            names: vec![],
        }
    }

    #[test]
    fn drops_oldest_when_over_capacity() {
        let mut b = JitterBuffer::new(3, 0);
        b.push(f(10));
        b.push(f(20));
        b.push(f(30));
        b.push(f(40));
        assert_eq!(b.len(), 3);
        assert_eq!(b.frames.front().unwrap().pts_ms, 20);
    }

    #[test]
    fn pop_due_respects_playout_delay() {
        let mut b = JitterBuffer::new(16, 100);
        b.push(f(50));
        b.push(f(120));
        b.push(f(200));
        let out = b.pop_due(150);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pts_ms, 50);
    }

    #[test]
    fn pop_due_drains_in_order() {
        let mut b = JitterBuffer::new(16, 0);
        b.push(f(10));
        b.push(f(20));
        b.push(f(30));
        let out = b.pop_due(25);
        assert_eq!(
            out.iter().map(|f| f.pts_ms).collect::<Vec<_>>(),
            vec![10, 20]
        );
        assert_eq!(b.len(), 1);
    }
}

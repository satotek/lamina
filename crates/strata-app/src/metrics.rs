//! Frame timing for the phase-0 performance overlay.
//!
//! The interval between two `render` calls covers everything a frame does:
//! element building, layout, painting and presentation. The time spent inside
//! `render` is tracked separately so a slow UI tree can be told apart from a
//! slow rasteriser.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const WINDOW: usize = 120;
/// Longer intervals are idle gaps, not slow frames: an app that only redraws on
/// input would otherwise report a meaningless average frame time. Excluded so
/// the overlay keeps measuring what it was opened to measure.
const IDLE_GAP_MS: f32 = 200.0;

#[derive(Clone, Copy, Default)]
pub struct FrameStats {
    /// Mean time between the starts of two frames, in milliseconds.
    pub frame_ms: f32,
    /// Mean time spent building the element tree, in milliseconds.
    pub render_ms: f32,
    /// 95th percentile of `frame_ms`, in milliseconds.
    pub p95_ms: f32,
    pub fps: f32,
    pub samples: usize,
}

pub struct FrameMetrics {
    last_frame_start: Option<Instant>,
    intervals: VecDeque<f32>,
    renders: VecDeque<f32>,
    frames: u64,
    next_log: u64,
}

impl Default for FrameMetrics {
    fn default() -> Self {
        Self {
            last_frame_start: None,
            intervals: VecDeque::with_capacity(WINDOW),
            renders: VecDeque::with_capacity(WINDOW),
            frames: 0,
            next_log: 60,
        }
    }
}

impl FrameMetrics {
    /// Call at the top of `render`. Returns the timestamp to pass to [`Self::end_frame`].
    pub fn begin_frame(&mut self) -> Instant {
        let now = Instant::now();
        if let Some(previous) = self.last_frame_start.replace(now) {
            let interval = elapsed_ms(previous.elapsed());
            if interval <= IDLE_GAP_MS {
                push(&mut self.intervals, interval);
            }
        }
        self.frames += 1;
        now
    }

    pub fn end_frame(&mut self, start: Instant) {
        push(&mut self.renders, elapsed_ms(start.elapsed()));
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// True once every `period` frames; used to keep the log readable.
    pub fn log_due(&mut self) -> bool {
        if self.frames >= self.next_log {
            self.next_log += 60;
            true
        } else {
            false
        }
    }

    pub fn stats(&self) -> FrameStats {
        let mut stats = FrameStats {
            samples: self.intervals.len(),
            ..FrameStats::default()
        };
        if self.intervals.is_empty() {
            return stats;
        }

        let frame_ms = mean(&self.intervals);
        let mut sorted: Vec<f32> = self.intervals.iter().copied().collect();
        sorted.sort_by(|a, b| a.total_cmp(b));

        stats.frame_ms = frame_ms;
        stats.render_ms = mean(&self.renders);
        stats.p95_ms = sorted[(sorted.len() - 1) * 95 / 100];
        stats.fps = if frame_ms > 0.0 {
            1000.0 / frame_ms
        } else {
            0.0
        };
        stats
    }
}

fn push(buffer: &mut VecDeque<f32>, value: f32) {
    if buffer.len() == WINDOW {
        buffer.pop_front();
    }
    buffer.push_back(value);
}

fn mean(values: &VecDeque<f32>) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f32>() / values.len() as f32
}

fn elapsed_ms(duration: Duration) -> f32 {
    duration.as_secs_f32() * 1000.0
}

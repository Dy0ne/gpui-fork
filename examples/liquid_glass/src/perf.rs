use std::{
    collections::VecDeque,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use crate::glass;

/// Frame intervals longer than this are idle gaps, not slow frames.
const IDLE_GAP: Duration = Duration::from_millis(250);
/// How many frame times the sparkline shows.
pub const HISTORY: usize = 60;
const SAMPLE_EVERY: Duration = Duration::from_millis(500);

/// Rolling frame and process statistics for the performance panel.
pub struct Perf {
    last_frame: Option<Instant>,
    /// Frame timestamps within the last second, for FPS.
    recent: VecDeque<Instant>,
    /// Recent frame times in milliseconds, oldest first.
    pub frame_ms: VecDeque<f32>,
    /// Smoothed glass shading time per frame, in milliseconds.
    pub glass_ms: f32,
    last_sample: Option<(Instant, Duration)>,
    /// Process CPU time as a percentage of one core; above 100 means more
    /// than one core is busy.
    pub cpu_percent: Option<f32>,
    pub memory_bytes: Option<u64>,
    pub threads: Option<u32>,
}

impl Perf {
    pub fn new() -> Self {
        Self {
            last_frame: None,
            recent: VecDeque::new(),
            frame_ms: VecDeque::with_capacity(HISTORY),
            glass_ms: 0.,
            last_sample: None,
            cpu_percent: None,
            memory_bytes: None,
            threads: None,
        }
    }

    /// Records a frame rendered at `now`.
    pub fn frame(&mut self, now: Instant) {
        if let Some(last) = self.last_frame {
            let interval = now - last;
            if interval < IDLE_GAP {
                if self.frame_ms.len() == HISTORY {
                    self.frame_ms.pop_front();
                }
                self.frame_ms.push_back(interval.as_secs_f32() * 1000.);
            }
        }
        self.last_frame = Some(now);
        self.recent.push_back(now);
        while self
            .recent
            .front()
            .is_some_and(|&t| now - t > Duration::from_secs(1))
        {
            self.recent.pop_front();
        }

        // Glass paints after `Demo::render`, so this is the previous frame's.
        let glass = glass::RENDER_NANOS.swap(0, Ordering::Relaxed) as f32 / 1e6;
        self.glass_ms += (glass - self.glass_ms) * 0.1;

        if self
            .last_sample
            .is_none_or(|(at, _)| now - at >= SAMPLE_EVERY)
        {
            self.sample(now);
        }
    }

    pub fn fps(&self) -> f32 {
        match (self.recent.front(), self.recent.back()) {
            (Some(&first), Some(&last)) if last > first => {
                (self.recent.len() - 1) as f32 / (last - first).as_secs_f32()
            }
            _ => 0.,
        }
    }

    pub fn frame_avg_max(&self) -> Option<(f32, f32)> {
        if self.frame_ms.is_empty() {
            return None;
        }
        let sum: f32 = self.frame_ms.iter().sum();
        let max = self.frame_ms.iter().copied().fold(0., f32::max);
        Some((sum / self.frame_ms.len() as f32, max))
    }

    fn sample(&mut self, now: Instant) {
        if let Some(cpu) = cpu_time() {
            if let Some((at, previous)) = self.last_sample {
                let wall = (now - at).as_secs_f32();
                if wall > 0. {
                    self.cpu_percent =
                        Some(cpu.saturating_sub(previous).as_secs_f32() / wall * 100.);
                }
            }
            self.last_sample = Some((now, cpu));
        }
        let (memory, threads) = memory_and_threads();
        self.memory_bytes = memory;
        self.threads = threads;
    }
}

/// User plus system CPU time used by this process so far.
#[cfg(unix)]
fn cpu_time() -> Option<Duration> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage fills the struct when it returns 0.
    let usage = unsafe {
        if libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) != 0 {
            return None;
        }
        usage.assume_init()
    };
    let time = |t: libc::timeval| {
        Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
    };
    Some(time(usage.ru_utime) + time(usage.ru_stime))
}

#[cfg(not(unix))]
fn cpu_time() -> Option<Duration> {
    None
}

/// Resident memory in bytes and thread count.
#[cfg(target_os = "macos")]
fn memory_and_threads() -> (Option<u64>, Option<u32>) {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: proc_pidinfo writes at most `size` bytes and returns how many
    // it wrote; the struct is only read when it was filled completely.
    let info = unsafe {
        let written = libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        );
        if written != size {
            return (None, None);
        }
        info.assume_init()
    };
    (
        Some(info.pti_resident_size),
        Some(info.pti_threadnum as u32),
    )
}

#[cfg(target_os = "linux")]
fn memory_and_threads() -> (Option<u64>, Option<u32>) {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return (None, None);
    };
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
    };
    (
        field("VmRSS:").map(|kb| kb * 1024),
        field("Threads:").map(|n| n as u32),
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn memory_and_threads() -> (Option<u64>, Option<u32>) {
    (None, None)
}

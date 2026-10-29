//! GPU timing behind `Window::gpu_frame_stats`. Off until the stats are first requested; then
//! every frame's command buffer reports its GPU start and end, and the backdrop layer passes
//! are timed with stage-boundary timestamp counters.

use std::{
    cell::RefCell,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result};
use block2::RcBlock;
use gpui::{DevicePixels, GpuFrameStats, Size};
use objc2::{Message, rc::Retained, runtime::ProtocolObject};
use objc2_foundation::NSRange;
use objc2_metal::{
    MTLClearColor, MTLCommandBuffer, MTLCounterDontSample, MTLCounterErrorValue,
    MTLCounterSampleBuffer, MTLCounterSampleBufferDescriptor, MTLCounterSamplingPoint,
    MTLCounterSet, MTLDevice, MTLLoadAction, MTLRenderCommandEncoder, MTLRenderPassDescriptor,
    MTLStorageMode, MTLStoreAction, MTLTexture, MTLViewport,
};
use parking_lot::Mutex;

/// Passes timed per frame: four per backdrop layer plus the final copy, so the layers of a frame
/// with more than 15 are only partly counted.
const MAX_TIMED_PASSES: usize = 61;
/// Frames that can be in flight at once (`CAMetalLayer`'s maximum drawable count), each needing
/// its own sample buffer until its command buffer completes.
const SAMPLE_BUFFERS: usize = 3;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Pass {
    /// Downsample or blur.
    BackdropBlur,
    BackdropComposite,
    BackdropCopy,
}

struct SampleBuffer {
    buffer: Retained<ProtocolObject<dyn MTLCounterSampleBuffer>>,
    in_flight: Arc<AtomicBool>,
}

struct Frame {
    sample_buffer: usize,
    passes: Vec<Pass>,
    layer_count: usize,
    /// CPU and GPU timestamps when the frame was encoded, to convert GPU ticks to time.
    start_timestamps: (u64, u64),
}

#[derive(Default)]
pub(crate) struct GpuTimer {
    enabled: bool,
    /// Empty when the GPU can't sample timestamps at stage boundaries; frame times still work.
    sample_buffers: Vec<SampleBuffer>,
    next_sample_buffer: usize,
    frame: RefCell<Option<Frame>>,
    latest: Arc<Mutex<Option<GpuFrameStats>>>,
}

impl GpuTimer {
    /// The last completed frame's stats. The first call turns timing on.
    pub(crate) fn stats(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
    ) -> Option<GpuFrameStats> {
        if !self.enabled {
            self.enabled = true;
            self.sample_buffers = new_sample_buffers(device);
        }
        *self.latest.lock()
    }

    pub(crate) fn begin_frame(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        layer_count: usize,
    ) {
        if !self.enabled {
            return;
        }
        // A frame that failed to render never reached `end_frame`; free its sample buffer.
        if let Some(stale) = self.frame.borrow_mut().take()
            && let Some(buffer) = self.sample_buffers.get(stale.sample_buffer)
        {
            buffer.in_flight.store(false, Ordering::Release);
        }
        // A frame whose sample buffer is still in use by an earlier one goes unmeasured.
        let sample_buffer = self.next_sample_buffer;
        if let Some(buffer) = self.sample_buffers.get(sample_buffer) {
            if buffer.in_flight.swap(true, Ordering::AcqRel) {
                return;
            }
            self.next_sample_buffer = (sample_buffer + 1) % self.sample_buffers.len();
        }
        *self.frame.borrow_mut() = Some(Frame {
            sample_buffer,
            passes: Vec::new(),
            layer_count,
            start_timestamps: sample_timestamps(device),
        });
    }

    /// Times the render pass `descriptor` describes, if this frame is measured and has samples
    /// left.
    pub(crate) fn time_pass(&self, descriptor: &MTLRenderPassDescriptor, pass: Pass) {
        let mut frame = self.frame.borrow_mut();
        let Some(frame) = frame.as_mut() else {
            return;
        };
        let Some(buffer) = self.sample_buffers.get(frame.sample_buffer) else {
            return;
        };
        if frame.passes.len() == MAX_TIMED_PASSES {
            return;
        }
        let first = 2 * frame.passes.len();
        frame.passes.push(pass);
        // Only the fragment stage: these passes draw a quad or two, and on tile-based GPUs the
        // vertex stage of a pass runs early, overlapping earlier passes, so timing from the
        // start of vertex work would count most of the frame for every pass.
        // SAFETY: Index zero is within the sample buffer attachment array, and both sample
        // indices are below the buffer's `2 * MAX_TIMED_PASSES` samples.
        unsafe {
            let attachment = descriptor
                .sampleBufferAttachments()
                .objectAtIndexedSubscript(0);
            attachment.setSampleBuffer(Some(&buffer.buffer));
            attachment.setStartOfVertexSampleIndex(MTLCounterDontSample);
            attachment.setEndOfVertexSampleIndex(MTLCounterDontSample);
            attachment.setStartOfFragmentSampleIndex(first);
            attachment.setEndOfFragmentSampleIndex(first + 1);
        }
    }

    /// Reads the frame's timings once `command_buffer` completes.
    pub(crate) fn end_frame(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
    ) {
        let Some(frame) = self.frame.borrow_mut().take() else {
            return;
        };
        let sample_buffer = self
            .sample_buffers
            .get(frame.sample_buffer)
            .map(|buffer| (buffer.buffer.clone(), buffer.in_flight.clone()));
        let device = device.retain();
        let latest = self.latest.clone();
        let block = RcBlock::new(
            move |command_buffer: ptr::NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                objc2::rc::autoreleasepool(|_| {
                    // SAFETY: Metal passes the command buffer that completed.
                    let command_buffer = unsafe { command_buffer.as_ref() };
                    let (start, end) = (command_buffer.GPUStartTime(), command_buffer.GPUEndTime());
                    let mut stats = GpuFrameStats {
                        frame: Duration::from_secs_f64((end - start).max(0.)),
                        backdrop_layer_count: frame.layer_count,
                        ..GpuFrameStats::default()
                    };
                    if let Some((buffer, in_flight)) = &sample_buffer {
                        let samples = resolve(buffer, 2 * frame.passes.len());
                        let nanos_per_tick =
                            nanos_per_tick(frame.start_timestamps, sample_timestamps(&device));
                        for (pass, pair) in frame.passes.iter().zip(samples.chunks_exact(2)) {
                            let (start, end) = (pair[0], pair[1]);
                            if start == MTLCounterErrorValue
                                || end == MTLCounterErrorValue
                                || end < start
                            {
                                continue;
                            }
                            let time = Duration::from_nanos(
                                ((end - start) as f64 * nanos_per_tick) as u64,
                            );
                            match pass {
                                Pass::BackdropBlur => stats.backdrop_blur += time,
                                Pass::BackdropComposite => stats.backdrop_composite += time,
                                Pass::BackdropCopy => stats.backdrop_copy += time,
                            }
                        }
                        in_flight.store(false, Ordering::Release);
                    }
                    *latest.lock() = Some(stats);
                });
            },
        );
        // SAFETY: Metal copies the block and invokes it once after this frame completes. Its
        // captures are owned, and the shared stats and sample buffer flag are synchronized.
        unsafe {
            command_buffer.addCompletedHandler(RcBlock::as_ptr(&block));
        }
    }
}

/// Builds a render pass like `new_command_encoder_for_texture` in `metal_renderer.rs`, timed when
/// `timer` measures this frame.
pub(crate) fn new_timed_command_encoder(
    timer: &GpuTimer,
    pass: Pass,
    command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
    texture: &ProtocolObject<dyn MTLTexture>,
    viewport_size: Size<DevicePixels>,
    load_action: MTLLoadAction,
) -> Result<Retained<ProtocolObject<dyn MTLRenderCommandEncoder>>> {
    let descriptor = MTLRenderPassDescriptor::new();
    // SAFETY: Index zero is within the render pass's color attachment array.
    let color_attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    color_attachment.setTexture(Some(texture));
    color_attachment.setStoreAction(MTLStoreAction::Store);
    color_attachment.setLoadAction(load_action);
    color_attachment.setClearColor(MTLClearColor {
        red: 0.,
        green: 0.,
        blue: 0.,
        alpha: 0.,
    });
    timer.time_pass(&descriptor, pass);

    let encoder = command_buffer
        .renderCommandEncoderWithDescriptor(&descriptor)
        .context("creating backdrop render command encoder")?;
    encoder.setViewport(MTLViewport {
        originX: 0.0,
        originY: 0.0,
        width: i32::from(viewport_size.width) as f64,
        height: i32::from(viewport_size.height) as f64,
        znear: 0.0,
        zfar: 1.0,
    });
    Ok(encoder)
}

fn new_sample_buffers(device: &ProtocolObject<dyn MTLDevice>) -> Vec<SampleBuffer> {
    if !device.supportsCounterSampling(MTLCounterSamplingPoint::AtStageBoundary) {
        return Vec::new();
    }
    let Some(timestamps) = device.counterSets().and_then(|sets| {
        sets.iter()
            .find(|set| set.name().to_string() == "timestamp")
    }) else {
        return Vec::new();
    };
    let descriptor = MTLCounterSampleBufferDescriptor::new();
    descriptor.setCounterSet(Some(&timestamps));
    // SAFETY: The count is positive and fits the timestamp counter set's limits.
    unsafe {
        descriptor.setSampleCount(2 * MAX_TIMED_PASSES);
    }
    descriptor.setStorageMode(MTLStorageMode::Shared);
    (0..SAMPLE_BUFFERS)
        .map_while(|_| {
            device
                .newCounterSampleBufferWithDescriptor_error(&descriptor)
                .ok()
        })
        .map(|buffer| SampleBuffer {
            buffer,
            in_flight: Arc::default(),
        })
        .collect()
}

/// The first `count` timestamps in `buffer`.
fn resolve(buffer: &ProtocolObject<dyn MTLCounterSampleBuffer>, count: usize) -> Vec<u64> {
    if count == 0 {
        return Vec::new();
    }
    // SAFETY: The range is within the buffer's sample count.
    let data = unsafe { buffer.resolveCounterRange(NSRange::new(0, count)) };
    data.map(|data| {
        data.to_vec()
            .chunks_exact(8)
            .map(|bytes| u64::from_ne_bytes(bytes.try_into().unwrap()))
            .collect()
    })
    .unwrap_or_default()
}

fn sample_timestamps(device: &ProtocolObject<dyn MTLDevice>) -> (u64, u64) {
    let (mut cpu, mut gpu) = (0, 0);
    // SAFETY: Both pointers are to live, writable locals.
    unsafe {
        device.sampleTimestamps_gpuTimestamp(
            ptr::NonNull::from(&mut cpu),
            ptr::NonNull::from(&mut gpu),
        );
    }
    (cpu, gpu)
}

/// GPU timestamps are in device ticks; CPU timestamps are in nanoseconds. On Apple silicon the
/// two match, but the ratio is measured rather than assumed.
fn nanos_per_tick(start: (u64, u64), end: (u64, u64)) -> f64 {
    let cpu = end.0.saturating_sub(start.0);
    let gpu = end.1.saturating_sub(start.1);
    if cpu == 0 || gpu == 0 {
        1.
    } else {
        cpu as f64 / gpu as f64
    }
}

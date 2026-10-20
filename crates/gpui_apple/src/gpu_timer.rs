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

use block2::RcBlock;
use foreign_types::ForeignTypeRef;
use gpui::{DevicePixels, GpuFrameStats, Size};
use metal::{COUNTER_DONT_SAMPLE, MTLCounterSamplingPoint, MTLStorageMode};
use objc2::{msg_send, rc::Retained, runtime::AnyObject};
use objc2_foundation::{NSData, NSRange};
use parking_lot::Mutex;

/// Passes timed per frame: four per backdrop layer plus the final copy, so the layers of a frame
/// with more than 15 are only partly counted.
const MAX_TIMED_PASSES: usize = 61;
/// Frames that can be in flight at once (`CAMetalLayer`'s maximum drawable count), each needing
/// its own sample buffer until its command buffer completes.
const SAMPLE_BUFFERS: usize = 3;
/// `MTLCounterErrorValue`: the GPU couldn't take that sample.
const COUNTER_ERROR: u64 = u64::MAX;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Pass {
    /// Downsample or blur.
    BackdropBlur,
    BackdropComposite,
    BackdropCopy,
}

struct SampleBuffer {
    buffer: metal::CounterSampleBuffer,
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
    pub(crate) fn stats(&mut self, device: &metal::Device) -> Option<GpuFrameStats> {
        if !self.enabled {
            self.enabled = true;
            self.sample_buffers = new_sample_buffers(device);
        }
        *self.latest.lock()
    }

    pub(crate) fn begin_frame(&mut self, device: &metal::Device, layer_count: usize) {
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
    pub(crate) fn time_pass(&self, descriptor: &metal::RenderPassDescriptorRef, pass: Pass) {
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
        let first = 2 * frame.passes.len() as u64;
        frame.passes.push(pass);
        // Only the fragment stage: these passes draw a quad or two, and on tile-based GPUs the
        // vertex stage of a pass runs early, overlapping earlier passes, so timing from the
        // start of vertex work would count most of the frame for every pass.
        let attachment = descriptor.sample_buffer_attachments().object_at(0).unwrap();
        attachment.set_sample_buffer(&buffer.buffer);
        attachment.set_start_of_vertex_sample_index(COUNTER_DONT_SAMPLE);
        attachment.set_end_of_vertex_sample_index(COUNTER_DONT_SAMPLE);
        attachment.set_start_of_fragment_sample_index(first);
        attachment.set_end_of_fragment_sample_index(first + 1);
    }

    /// Reads the frame's timings once `command_buffer` completes.
    pub(crate) fn end_frame(
        &mut self,
        device: &metal::Device,
        command_buffer: &metal::CommandBufferRef,
    ) {
        let Some(frame) = self.frame.borrow_mut().take() else {
            return;
        };
        let sample_buffer = self
            .sample_buffers
            .get(frame.sample_buffer)
            .map(|buffer| (buffer.buffer.clone(), buffer.in_flight.clone()));
        let device = device.clone();
        let latest = self.latest.clone();
        let block = RcBlock::new(move |command_buffer: ptr::NonNull<AnyObject>| {
            objc2::rc::autoreleasepool(|_| {
                // SAFETY: Metal passes the command buffer that completed.
                let command_buffer = unsafe { command_buffer.as_ref() };
                let (start, end): (f64, f64) = unsafe {
                    (
                        msg_send![command_buffer, GPUStartTime],
                        msg_send![command_buffer, GPUEndTime],
                    )
                };
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
                        if start == COUNTER_ERROR || end == COUNTER_ERROR || end < start {
                            continue;
                        }
                        let time =
                            Duration::from_nanos(((end - start) as f64 * nanos_per_tick) as u64);
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
        });
        // SAFETY: Both pointee types are opaque views of the same Objective-C block pointer ABI.
        unsafe {
            command_buffer.add_completed_handler(&*RcBlock::as_ptr(&block).cast());
        }
    }
}

/// Builds a render pass like `new_command_encoder_for_texture` in `metal_renderer.rs`, timed when
/// `timer` measures this frame.
pub(crate) fn new_timed_command_encoder<'a>(
    timer: &GpuTimer,
    pass: Pass,
    command_buffer: &'a metal::CommandBufferRef,
    texture: &'a metal::TextureRef,
    viewport_size: Size<DevicePixels>,
    clear_color: Option<metal::MTLClearColor>,
) -> &'a metal::RenderCommandEncoderRef {
    let descriptor = metal::RenderPassDescriptor::new();
    let color_attachment = descriptor.color_attachments().object_at(0).unwrap();
    color_attachment.set_texture(Some(texture));
    color_attachment.set_store_action(metal::MTLStoreAction::Store);
    if let Some(clear_color) = clear_color {
        color_attachment.set_load_action(metal::MTLLoadAction::Clear);
        color_attachment.set_clear_color(clear_color);
    } else {
        color_attachment.set_load_action(metal::MTLLoadAction::Load);
    }
    timer.time_pass(descriptor, pass);

    let encoder = command_buffer.new_render_command_encoder(descriptor);
    encoder.set_viewport(metal::MTLViewport {
        originX: 0.0,
        originY: 0.0,
        width: i32::from(viewport_size.width) as f64,
        height: i32::from(viewport_size.height) as f64,
        znear: 0.0,
        zfar: 1.0,
    });
    encoder
}

fn new_sample_buffers(device: &metal::Device) -> Vec<SampleBuffer> {
    if !device.supports_counter_sampling(MTLCounterSamplingPoint::AtStageBoundary) {
        return Vec::new();
    }
    let Some(timestamps) = device
        .counter_sets()
        .into_iter()
        .find(|set| set.name() == "timestamp")
    else {
        return Vec::new();
    };
    let descriptor = metal::CounterSampleBufferDescriptor::new();
    descriptor.set_counter_set(&timestamps);
    descriptor.set_sample_count(2 * MAX_TIMED_PASSES as u64);
    descriptor.set_storage_mode(MTLStorageMode::Shared);
    (0..SAMPLE_BUFFERS)
        .map_while(|_| {
            device
                .new_counter_sample_buffer_with_descriptor(&descriptor)
                .ok()
        })
        .map(|buffer| SampleBuffer {
            buffer,
            in_flight: Arc::default(),
        })
        .collect()
}

/// The first `count` timestamps in `buffer`. `metal`'s own `resolve_counter_range` copies zero
/// bytes (it sizes the copy from the still-empty `Vec`), so this calls Metal directly.
fn resolve(buffer: &metal::CounterSampleBufferRef, count: usize) -> Vec<u64> {
    if count == 0 {
        return Vec::new();
    }
    // SAFETY: `buffer` is a live MTLCounterSampleBuffer, and the range is within its sample count.
    let data: Option<Retained<NSData>> = unsafe {
        let buffer = &*buffer.as_ptr().cast::<AnyObject>();
        msg_send![buffer, resolveCounterRange: NSRange::new(0, count)]
    };
    data.map(|data| {
        data.to_vec()
            .chunks_exact(8)
            .map(|bytes| u64::from_ne_bytes(bytes.try_into().unwrap()))
            .collect()
    })
    .unwrap_or_default()
}

fn sample_timestamps(device: &metal::Device) -> (u64, u64) {
    let (mut cpu, mut gpu) = (0, 0);
    device.sample_timestamps(&mut cpu, &mut gpu);
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

use std::ffi::c_void;
use std::io;
use std::ptr::{null, null_mut, NonNull};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use openairplay1::AudioSink;

const BUFFERS: usize = 4;
const SAMPLES_PER_BUFFER: usize = 2048;
type Queue = *mut c_void;

#[repr(C)]
struct Format {
    sample_rate: f64,
    format_id: u32,
    flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

// Only the fixed prefix of AudioQueueBuffer is used.
#[repr(C)]
struct Buffer {
    capacity: u32,
    data: *mut c_void,
    size: u32,
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioQueueNewOutput(
        format: *const Format,
        callback: unsafe extern "C" fn(*mut c_void, Queue, *mut Buffer),
        context: *mut c_void,
        run_loop: *const c_void,
        mode: *const c_void,
        flags: u32,
        queue: *mut Queue,
    ) -> i32;
    fn AudioQueueAllocateBuffer(queue: Queue, size: u32, buffer: *mut *mut Buffer) -> i32;
    fn AudioQueueEnqueueBuffer(
        queue: Queue,
        buffer: *mut Buffer,
        count: u32,
        packets: *const c_void,
    ) -> i32;
    fn AudioQueueSetParameter(queue: Queue, parameter: u32, value: f32) -> i32;
    fn AudioQueueStart(queue: Queue, time: *const c_void) -> i32;
    fn AudioQueueDispose(queue: Queue, immediate: u8) -> i32;
}

unsafe extern "C" fn returned(context: *mut c_void, _queue: Queue, buffer: *mut Buffer) {
    // The callback context remains alive until AudioQueueDispose completes.
    let sender = unsafe { &*context.cast::<SyncSender<usize>>() };
    let _ = sender.try_send(buffer as usize);
}

struct Output {
    queue: NonNull<c_void>,
    _sender: Box<SyncSender<usize>>,
    free: Receiver<usize>,
    started: bool,
}

// Only the playback thread uses the queue. The system callback owns no Rust state
// beyond its synchronized buffer sender, which the queue keeps alive.
unsafe impl Send for Output {}

impl Output {
    fn new(rate: u32, channels: u8) -> io::Result<Self> {
        let bytes_per_frame = u32::from(channels) * 2;
        let format = Format {
            sample_rate: rate.into(),
            format_id: u32::from_be_bytes(*b"lpcm"),
            flags: 4 | 8, // Signed integer, packed.
            bytes_per_packet: bytes_per_frame,
            frames_per_packet: 1,
            bytes_per_frame,
            channels_per_frame: channels.into(),
            bits_per_channel: 16,
            reserved: 0,
        };
        let (sender, free) = sync_channel(BUFFERS);
        let mut sender = Box::new(sender);
        let mut queue = null_mut();
        // Core Audio copies the format and borrows the stable callback context.
        check(unsafe {
            AudioQueueNewOutput(
                &format,
                returned,
                (&mut *sender as *mut SyncSender<usize>).cast(),
                null(),
                null(),
                0,
                &mut queue,
            )
        })?;
        let output = Self {
            queue: NonNull::new(queue)
                .ok_or_else(|| io::Error::other("Audio output is unavailable"))?,
            _sender: sender,
            free,
            started: false,
        };
        for _ in 0..BUFFERS {
            let mut buffer = null_mut();
            // AudioQueue owns its buffers until disposal.
            check(unsafe {
                AudioQueueAllocateBuffer(queue, (2 * SAMPLES_PER_BUFFER) as u32, &mut buffer)
            })?;
            output
                ._sender
                .try_send(buffer as usize)
                .map_err(io::Error::other)?;
        }
        Ok(output)
    }

    fn enqueue(&mut self, pcm: &[i16], volume: f32) -> io::Result<()> {
        let buffer = self
            .free
            .recv_timeout(Duration::from_secs(1))
            .map_err(io::Error::other)?;
        let buffer = NonNull::new(buffer as *mut Buffer)
            .ok_or_else(|| io::Error::other("The audio queue returned an empty buffer"))?;
        let queue = self.queue.as_ptr();
        let size = std::mem::size_of_val(pcm);
        // A returned buffer belongs to this thread until it is enqueued again.
        unsafe {
            if size > (*buffer.as_ptr()).capacity as usize {
                return Err(io::Error::other("The audio buffer is too small"));
            }
            std::ptr::copy_nonoverlapping(
                pcm.as_ptr().cast::<u8>(),
                (*buffer.as_ptr()).data.cast(),
                size,
            );
            (*buffer.as_ptr()).size = size as u32;
            check(AudioQueueSetParameter(queue, 1, volume))?;
            check(AudioQueueEnqueueBuffer(queue, buffer.as_ptr(), 0, null()))?;
            if !self.started {
                check(AudioQueueStart(queue, null()))?;
                self.started = true;
            }
        }
        Ok(())
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // Immediate disposal stops playback and finishes any callbacks before the
        // sender and the queue's native buffers are released.
        unsafe { AudioQueueDispose(self.queue.as_ptr(), 1) };
    }
}

pub(super) struct Speaker {
    rate: u32,
    channels: u8,
    volume: Arc<AtomicU32>,
    output: Option<Output>,
}

impl Speaker {
    pub(super) fn new(rate: u32, channels: u8, volume: Arc<AtomicU32>) -> Self {
        Self {
            rate,
            channels,
            volume,
            output: None,
        }
    }
}

impl AudioSink for Speaker {
    fn write(&mut self, pcm: &[i16]) {
        if self.output.is_none() {
            self.output = Output::new(self.rate, self.channels).ok();
        }
        let Some(output) = self.output.as_mut() else {
            std::thread::sleep(Duration::from_secs_f64(
                pcm.len() as f64 / (f64::from(self.rate) * f64::from(self.channels)).max(1.0),
            ));
            return;
        };
        let volume = f32::from_bits(self.volume.load(Ordering::Relaxed));
        for chunk in pcm.chunks(SAMPLES_PER_BUFFER) {
            if output.enqueue(chunk, volume).is_err() {
                self.flush();
                return;
            }
        }
    }

    fn flush(&mut self) {
        self.output = None;
    }
}

fn check(status: i32) -> io::Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!("Core Audio error {status}")))
    }
}

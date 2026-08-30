use std::ptr::NonNull;

use pipewire as pw;
use pw::spa;

/// A dequeued PipeWire buffer that is always returned to its originating stream.
pub(crate) struct DequeuedBuffer<'a> {
    stream: &'a pw::stream::Stream,
    raw: NonNull<pw::sys::pw_buffer>,
}

impl<'a> DequeuedBuffer<'a> {
    pub(crate) fn dequeue(stream: &'a pw::stream::Stream) -> Option<Self> {
        // SAFETY: PipeWire owns the returned buffer. The guard retains the
        // originating stream and queues this exact pointer again in Drop.
        NonNull::new(unsafe { stream.dequeue_raw_buffer() }).map(|raw| Self { stream, raw })
    }

    fn spa_buffer(&self) -> Option<NonNull<spa::sys::spa_buffer>> {
        // SAFETY: The pw_buffer remains dequeued and alive for the guard's
        // lifetime. The first field is the SPA buffer pointer on every
        // supported PipeWire ABI.
        NonNull::new(unsafe { self.raw.as_ref().buffer })
    }

    pub(crate) fn datas_mut(&mut self) -> &mut [spa::buffer::Data] {
        let Some(mut buffer) = self.spa_buffer() else {
            return &mut [];
        };
        // SAFETY: PipeWire owns an array of n_datas SPA data records for as
        // long as this buffer is dequeued. Data is a transparent wrapper.
        let buffer = unsafe { buffer.as_mut() };
        if buffer.n_datas == 0 || buffer.datas.is_null() {
            return &mut [];
        }
        let Ok(length) = usize::try_from(buffer.n_datas) else {
            return &mut [];
        };
        // SAFETY: The null and length checks above match spa_buffer's contract.
        unsafe { std::slice::from_raw_parts_mut(buffer.datas.cast::<spa::buffer::Data>(), length) }
    }

    pub(crate) fn metadata(&self, metadata_type: u32) -> Option<&[u8]> {
        let buffer = self.spa_buffer()?;
        // SAFETY: The SPA buffer is valid while dequeued. The returned meta is
        // owned by that buffer and is only borrowed for the guard's lifetime.
        let metadata =
            unsafe { spa::sys::spa_buffer_find_meta(buffer.as_ptr(), metadata_type).as_ref() }?;
        if metadata.data.is_null() || metadata.size == 0 {
            return None;
        }
        let length = usize::try_from(metadata.size).ok()?;
        // SAFETY: spa_meta describes a data allocation of exactly `size` bytes
        // whose lifetime is tied to the dequeued buffer.
        Some(unsafe { std::slice::from_raw_parts(metadata.data.cast::<u8>(), length) })
    }
}

impl Drop for DequeuedBuffer<'_> {
    fn drop(&mut self) {
        // SAFETY: `raw` came from this stream's dequeue call and this guard
        // queues it exactly once.
        unsafe { self.stream.queue_raw_buffer(self.raw.as_ptr()) };
    }
}

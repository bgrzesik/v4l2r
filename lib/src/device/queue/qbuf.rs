//! Provides types related to queuing buffers on a `Queue` object.
use super::{buffer::BufferInfo, Capture, Direction, Output};
use super::{BufferState, BufferStateFuse, BuffersAllocated, Queue};
use crate::ioctl::{self, QBufIoctlError, QBufResult};
use crate::memory::*;
use std::convert::Infallible;
use std::{
    fmt::{self, Debug},
    sync::Arc,
};

use nix::sys::time::{TimeVal, TimeValLike};
use thiserror::Error;

pub mod get_free;
pub mod get_indexed;

/// Error that can occur when queuing a buffer. It wraps a regular error and also
/// returns the plane handles back to the user.
#[derive(Error)]
#[error("{}", self.error)]
pub struct QueueError<P: BufferHandles> {
    pub error: ioctl::QBufError<Infallible>,
    pub plane_handles: P,
}

impl<P: BufferHandles> Debug for QueueError<P> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Debug::fmt(&self.error, f)
    }
}

#[allow(type_alias_bounds)]
pub type QueueResult<R, P: BufferHandles> = std::result::Result<R, QueueError<P>>;

/// A free buffer that has just been obtained from `Queue::get_buffer()` and
/// which is being prepared to the queued.
///
/// The necessary setup depends on the kind of direction of the buffer:
///
/// * Capture buffers are to be filled by the driver, so we just need to attach
///   one memory handle per plane before submitting them (MMAP buffers don't
///   need this step).
/// * Output buffers on the other hand are filled by us ; so on top of one valid
///   memory handle per plane, we also need to specify how much data we have
///   written in each of them, and possibly set a few flags on the buffer.
///
/// This struct is specialized on both the direction and type of memory so
/// mandatory data is always specified, and irrelevant data is inaccessible.
///
/// Once a buffer is ready, it can be queued using the queue() method. Failures
/// occur if the QBUF ioctl failed, or if the number of specified planes does
/// not match the number of planes in the format. A queued buffer remains
/// inaccessible for further queuing until it has been dequeued and dropped.
///
/// If a QBuffer object is destroyed before being queued, its buffer returns
/// to the pool of available buffers and can be requested again with
/// `Queue::get_buffer()`.
///
/// A QBuffer holds a strong reference to its queue, therefore the state of the
/// queue or device cannot be changed while it is being used. Contrary to
/// DQBuffer which can be freely duplicated and passed around, instances of this
/// struct are supposed to be short-lived.
pub struct QBuffer<'a, D: Direction, P: PrimitiveBufferHandles, Q: BufferHandles + From<P>> {
    queue: &'a Queue<D, BuffersAllocated<Q>>,
    index: usize,
    num_planes: usize,
    timestamp: TimeVal,
    fuse: BufferStateFuse<Q>,
    _p: std::marker::PhantomData<P>,
}

impl<'a, D: Direction, P: PrimitiveBufferHandles, Q: BufferHandles + From<P>> QBuffer<'a, D, P, Q> {
    pub(super) fn new(
        queue: &'a Queue<D, BuffersAllocated<Q>>,
        buffer_info: &Arc<BufferInfo<Q>>,
    ) -> Self {
        let buffer = &buffer_info.features;
        let fuse = BufferStateFuse::new(Arc::downgrade(buffer_info));

        QBuffer {
            queue,
            index: buffer.index,
            num_planes: buffer.planes.len(),
            timestamp: TimeVal::zero(),
            fuse,
            _p: std::marker::PhantomData,
        }
    }

    /// Returns the V4L2 index of this buffer.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Returns the number of handles/plane data expected to be specified for
    /// this buffer.
    pub fn num_expected_planes(&self) -> usize {
        self.num_planes
    }

    pub fn set_timestamp(mut self, timestamp: TimeVal) -> Self {
        self.timestamp = timestamp;
        self
    }

    // R is meant to mean "either P or Q".
    // Caller is responsible for making sure that the number of planes and
    // plane_handles is the same as the number of expected planes for this
    // buffer.
    fn queue_bound_planes<R: BufferHandles + Into<Q>>(
        mut self,
        planes: Vec<ioctl::QBufPlane>,
        plane_handles: R,
    ) -> QueueResult<(), R> {
        let mut qbuffer =
            ioctl::QBuffer::<P::HandleType>::new(self.queue.inner.type_, self.index as u32);
        qbuffer.planes = planes;
        qbuffer.timestamp = self.timestamp;

        match ioctl::qbuf(&self.queue.inner, qbuffer) {
            Ok(()) => (),
            Err(error) => {
                return Err(QueueError {
                    error,
                    plane_handles,
                })
            }
        };

        // We got this now.
        self.fuse.disarm();

        self.queue
            .state
            .buffer_info
            .get(self.index)
            .expect("Inconsistent buffer state!")
            .update_state(|state| {
                *state = BufferState::Queued(plane_handles.into());
            });

        Ok(())
    }
}

impl<'a, P, Q> QBuffer<'a, Output, P, Q>
where
    P: PrimitiveBufferHandles,
    P::HandleType: Mappable,
    Q: BufferHandles + From<P>,
{
    pub fn get_plane_mapping(&self, plane: usize) -> Option<ioctl::PlaneMapping> {
        let buffer_info = self.queue.state.buffer_info.get(self.index)?;
        let plane_info = buffer_info.features.planes.get(plane)?;
        P::HandleType::map(self.queue.inner.device.as_ref(), plane_info)
    }
}

/// Trait for queueable CAPTURE buffers. These buffers only require handles to
/// be queued.
pub trait CaptureQueueable<Q: BufferHandles> {
    /// Queue the buffer after binding `handles`, consuming the object.
    /// The number of handles must match the buffer's expected number of planes.
    fn queue_with_handles(self, handles: Q) -> QueueResult<(), Q>;
}

/// Trait for queueable OUTPUT buffers. The number of bytes used must be
/// specified for each plane.
pub trait OutputQueueable<Q: BufferHandles> {
    /// Queue the buffer after binding `handles`, consuming the object.
    /// The number of handles must match the buffer's expected number of planes.
    /// `bytes_used` must be a slice with as many slices as there are handles,
    /// describing the amount of useful data in each of them.
    fn queue_with_handles(self, handles: Q, bytes_used: &[usize]) -> QueueResult<(), Q>;
}

/// Trait for all objects that are capable of providing objects that can be
/// queued to the CAPTURE queue.
pub trait CaptureQueueableProvider<'a, Q: BufferHandles> {
    type Queueable: 'a + CaptureQueueable<Q>;
}

/// Trait for all objects that are capable of providing objects that can be
/// queued to the CAPTURE queue.
pub trait OutputQueueableProvider<'a, Q: BufferHandles> {
    type Queueable: 'a + OutputQueueable<Q>;
}

/// Any CAPTURE QBuffer implements CaptureQueueable.
impl<P: PrimitiveBufferHandles, Q: BufferHandles + From<P>> CaptureQueueable<Q>
    for QBuffer<'_, Capture, P, Q>
{
    fn queue_with_handles(self, handles: Q) -> QueueResult<(), Q> {
        if handles.len() != self.num_expected_planes() {
            return Err(QueueError {
                error: QBufIoctlError::NumPlanesMismatch(handles.len(), self.num_expected_planes())
                    .into(),
                plane_handles: handles,
            });
        }

        // TODO BufferHandles should have a method returning the actual MEMORY_TYPE implemented? So we can check
        // that it matches with P.

        let planes: Vec<_> = (0..self.num_expected_planes())
            .map(|i| {
                let mut plane = ioctl::QBufPlane::new(0);
                handles.fill_v4l2_plane(i, &mut plane.0);
                plane
            })
            .collect();

        self.queue_bound_planes(planes, handles)
    }
}

/// Any OUTPUT QBuffer implements OutputQueueable.
impl<P: PrimitiveBufferHandles, Q: BufferHandles + From<P>> OutputQueueable<Q>
    for QBuffer<'_, Output, P, Q>
{
    fn queue_with_handles(self, handles: Q, bytes_used: &[usize]) -> QueueResult<(), Q> {
        if handles.len() != self.num_expected_planes() {
            return Err(QueueError {
                error: QBufIoctlError::NumPlanesMismatch(handles.len(), self.num_expected_planes())
                    .into(),
                plane_handles: handles,
            });
        }

        // TODO make specific error for bytes_used?
        if bytes_used.len() != self.num_expected_planes() {
            return Err(QueueError {
                error: QBufIoctlError::NumPlanesMismatch(
                    bytes_used.len(),
                    self.num_expected_planes(),
                )
                .into(),
                plane_handles: handles,
            });
        }

        // TODO BufferHandles should have a method returning the actual MEMORY_TYPE implemented? So we can check
        // that it matches with P.

        let planes: Vec<_> = bytes_used
            .iter()
            .enumerate()
            .map(|(i, size)| {
                let mut plane = ioctl::QBufPlane::new(*size);
                handles.fill_v4l2_plane(i, &mut plane.0);
                plane
            })
            .collect();

        self.queue_bound_planes(planes, handles)
    }
}

/// Shortcut to quickly queue self-backed CAPTURE buffers without specifying
/// empty handles.
/// Since we don't receive plane handles, we also don't need to return any, so
/// the returned error can be simplified.
impl<P: PrimitiveBufferHandles + Default, Q: BufferHandles + From<P>> QBuffer<'_, Capture, P, Q>
where
    <P::HandleType as PlaneHandle>::Memory: SelfBacked,
{
    pub fn queue(self) -> QBufResult<(), Infallible> {
        let planes: Vec<_> = (0..self.num_expected_planes())
            .map(|_| ioctl::QBufPlane::new(0))
            .collect();

        self.queue_bound_planes::<P>(planes, Default::default())
            .map_err(|e| e.error)
    }
}

/// Shortcut to quickly queue self-backed OUTPUT buffers without specifying
/// empty handles.
/// Since we don't receive plane handles, we also don't need to return any, so
/// the returned error can be simplified.
impl<P: PrimitiveBufferHandles + Default, Q: BufferHandles + From<P>> QBuffer<'_, Output, P, Q>
where
    <P::HandleType as PlaneHandle>::Memory: SelfBacked,
{
    pub fn queue(self, bytes_used: &[usize]) -> QBufResult<(), Infallible> {
        // TODO make specific error for bytes_used?
        if bytes_used.len() != self.num_expected_planes() {
            return Err(QBufIoctlError::NumPlanesMismatch(
                bytes_used.len(),
                self.num_expected_planes(),
            )
            .into());
        }

        let planes: Vec<_> = bytes_used
            .iter()
            .map(|size| ioctl::QBufPlane::new(*size))
            .collect();

        self.queue_bound_planes::<P>(planes, Default::default())
            .map_err(|e| e.error)
    }
}

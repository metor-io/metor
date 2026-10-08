//! `DeviceSide<K>`, the end of a simulated resource that a model system
//! serves.

use core::marker::PhantomData;

use crate::port::{Input, Output, SendError};

use super::cap::Interface;
use super::sim::Held;
use super::{BufRef, Cqe, IoError, Sqe, Tables};

/// The device end of a simulated resource: requests from drivers come in,
/// and the model writes their replies.
pub struct DeviceSide<K: Interface> {
    pub(super) requests: Input<Sqe>,
    pub(super) replies: Output<Cqe>,
    tables: Tables,
    pub(super) held: Held,
    /// How many times `serve` has run.
    pub(super) serves: u64,
    _k: PhantomData<K>,
}

/// The interface of a `DeviceSide` bound before its parameter types it.
pub(crate) struct Erased;

impl Interface for Erased {
    const NAME: &'static str = "";
}

impl DeviceSide<Erased> {
    /// A device end whose `serve` holds at most `held` ops.
    pub(crate) fn new(
        requests: Input<Sqe>,
        replies: Output<Cqe>,
        tables: Tables,
        held: usize,
    ) -> Self {
        Self {
            requests,
            replies,
            tables,
            held: Held::new(held),
            serves: 0,
            _k: PhantomData,
        }
    }

    pub(crate) fn typed<K: Interface>(self) -> DeviceSide<K> {
        DeviceSide {
            requests: self.requests,
            replies: self.replies,
            tables: self.tables,
            held: self.held,
            serves: self.serves,
            _k: PhantomData,
        }
    }
}

impl<K: Interface> DeviceSide<K> {
    /// Every request that has arrived. Skips records that do not decode.
    pub fn requests(&mut self) -> impl Iterator<Item = Sqe> + '_ {
        self.requests.drain().flatten()
    }

    /// The bytes a driver's buffer holds, such as a write's payload.
    pub fn bytes(&mut self, buf: BufRef) -> Result<&mut [u8], IoError> {
        let raw = self.tables.raw(buf).ok_or(IoError::BadBuf)?;
        // SAFETY: the driver lent this slot to the request until its reply is
        // written, and models run on the coordinator thread, so nothing else
        // touches it. `&mut self` keeps two slices from coexisting.
        Ok(unsafe { core::slice::from_raw_parts_mut(raw.ptr.as_ptr(), raw.len as usize) })
    }

    /// A driver's whole buffer, for a read to fill. Report the bytes filled in
    /// the reply's `Out::Len`.
    pub fn space(&mut self, buf: BufRef) -> Result<&mut [u8], IoError> {
        let raw = self.tables.raw(buf).ok_or(IoError::BadBuf)?;
        // SAFETY: as in `bytes`.
        Ok(unsafe { core::slice::from_raw_parts_mut(raw.ptr.as_ptr(), raw.cap as usize) })
    }

    pub fn reply(&self, cqe: Cqe) -> Result<(), SendError> {
        self.replies.write(&cqe)
    }
}

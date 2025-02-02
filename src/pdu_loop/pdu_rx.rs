use super::storage::PduStorageRef;
use crate::{
    command::CommandCode,
    error::{Error, PduError, PduValidationError},
    pdu_loop::{frame_header::FrameHeader, pdu_flags::PduFlags},
    ETHERCAT_ETHERTYPE, MASTER_ADDR,
};
use nom::{
    bytes::complete::take,
    combinator::map_res,
    error::context,
    number::complete::{le_u16, u8},
};
use packed_struct::PackedStructSlice;
use smoltcp::wire::EthernetFrame;

/// Receive frames from network interface.
pub struct PduRx<'sto> {
    storage: PduStorageRef<'sto>,
}

// SAFETY: We're tied to the lifetime of the backing storage with 'sto.
unsafe impl<'sto> Send for PduRx<'sto> {}
unsafe impl<'sto> Sync for PduRx<'sto> {}

impl<'sto> PduRx<'sto> {
    pub(in crate::pdu_loop) fn new(storage: PduStorageRef<'sto>) -> Self {
        Self { storage }
    }

    /// Parse a PDU from a complete Ethernet II frame.
    // NOTE: &mut self so this struct can only be used in one place.
    pub fn receive_frame(&mut self, ethernet_frame: &[u8]) -> Result<(), Error> {
        let raw_packet = EthernetFrame::new_checked(ethernet_frame)?;

        // Look for EtherCAT packets whilst ignoring broadcast packets sent from self. As per
        // <https://github.com/OpenEtherCATsociety/SOEM/issues/585#issuecomment-1013688786>, the
        // first slave will set the second bit of the MSB of the MAC address (U/L bit). This means
        // if we send e.g. 10:10:10:10:10:10, we receive 12:10:10:10:10:10 which is useful for this
        // filtering.
        if raw_packet.ethertype() != ETHERCAT_ETHERTYPE || raw_packet.src_addr() == MASTER_ADDR {
            return Ok(());
        }

        let i = raw_packet.payload();

        let (i, header) = context("header", FrameHeader::parse)(i)?;

        // Only take as much as the header says we should
        let (_rest, i) = take(header.payload_len())(i)?;

        let (i, command_code) = map_res(u8, CommandCode::try_from)(i)?;
        let (i, index) = u8(i)?;

        let mut frame = self
            .storage
            .get_receiving(index)
            .ok_or_else(|| PduError::InvalidIndex(usize::from(index)))?;

        if frame.index() != index {
            return Err(Error::Pdu(PduError::Validation(
                PduValidationError::IndexMismatch {
                    sent: frame.index(),
                    received: index,
                },
            )));
        }

        let (i, command) = command_code.parse_address(i)?;

        // Check for weird bugs where a slave might return a different command than the one sent for
        // this PDU index.
        if command.code() != frame.command().code() {
            return Err(Error::Pdu(PduError::Validation(
                PduValidationError::CommandMismatch {
                    sent: command,
                    received: frame.command(),
                },
            )));
        }

        let (i, flags) = map_res(take(2usize), PduFlags::unpack_from_slice)(i)?;
        let (i, irq) = le_u16(i)?;
        let (i, data) = take(flags.length)(i)?;
        let (i, working_counter) = le_u16(i)?;

        log::trace!("Received frame with index {index:#04x}, WKC {working_counter}");

        // `_i` should be empty as we `take()`d an exact amount above.
        debug_assert_eq!(i.len(), 0, "trailing data in received frame");

        let frame_data = frame.buf_mut();

        frame_data[0..usize::from(flags.len())].copy_from_slice(data);

        // FIXME: Release frame if any of this method fails

        frame.mark_received(flags, irq, working_counter)?;

        Ok(())
    }
}

//! Items to use when not in `no_std` environments.

mod raw_socket;

use self::raw_socket::RawSocketDesc;
use crate::{
    error::Error,
    pdu_loop::{PduRx, PduTx},
};
use async_io::Async;
use core::{future::Future, pin::Pin, task::Poll};
use futures_lite::io::{AsyncRead, AsyncWrite};

struct TxRxFut<'a> {
    socket: Async<RawSocketDesc>,
    tx: PduTx<'a>,
    rx: PduRx<'a>,
}

impl Future for TxRxFut<'_> {
    type Output = Result<(), Error>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut core::task::Context<'_>) -> Poll<Self::Output> {
        let mut buf = [0u8; 1536];

        // Lock the waker so we don't poll concurrently. spin::RwLock does this atomically
        let mut waker = self.tx.lock_waker();

        while let Some(frame) = self.tx.next_sendable_frame() {
            // FIXME: Release frame on failure
            let data = frame.write_ethernet_packet(&mut buf)?;

            #[cfg(feature = "bench-hacks")]
            {
                // Epic hack to make data writable
                let data: &mut [u8] = unsafe {
                    core::slice::from_raw_parts_mut(data.as_ptr() as *mut u8, data.len())
                };

                let mut frame = smoltcp::wire::EthernetFrame::new_unchecked(data);

                // For benchmarks, change the first octet of the source MAC address so the packet
                // isn't filtered out as a sent frame but is treated as a received frame instead.
                frame.set_src_addr(smoltcp::wire::EthernetAddress([
                    0x12, 0x10, 0x10, 0x10, 0x10, 0x10,
                ]))
            }

            match Pin::new(&mut self.socket).poll_write(ctx, data) {
                Poll::Ready(Ok(bytes_written)) => {
                    if bytes_written != data.len() {
                        log::error!("Only wrote {} of {} bytes", bytes_written, data.len());

                        // FIXME: Release frame

                        return Poll::Ready(Err(Error::PartialSend {
                            len: data.len(),
                            sent: bytes_written,
                        }));
                    }

                    frame.mark_sent();
                }
                // FIXME: Release frame on failure
                Poll::Ready(Err(e)) => {
                    log::error!("Send PDU failed: {e}");

                    return Poll::Ready(Err(Error::SendFrame));
                }
                Poll::Pending => (),
            }
        }

        match Pin::new(&mut self.socket).poll_read(ctx, &mut buf) {
            Poll::Ready(Ok(n)) => {
                let packet = &buf[0..n];

                // FIXME: Release frame on failure
                if let Err(e) = self.rx.receive_frame(packet) {
                    log::error!("Failed to receive frame: {}", e);

                    return Poll::Ready(Err(Error::ReceiveFrame));
                }
            }
            // FIXME: Release frame on failure
            Poll::Ready(Err(e)) => {
                log::error!("Receive PDU failed: {e}");

                return Poll::Ready(Err(Error::ReceiveFrame));
            }
            Poll::Pending => (),
        }

        waker.replace(ctx.waker().clone());

        Poll::Pending
    }
}

/// Spawn a TX and RX task.
pub fn tx_rx_task(
    interface: &str,
    pdu_tx: PduTx<'static>,
    pdu_rx: PduRx<'static>,
) -> Result<impl Future<Output = Result<(), Error>>, std::io::Error> {
    let socket = RawSocketDesc::new(interface)?;

    let async_socket = async_io::Async::new(socket)?;

    let task = TxRxFut {
        socket: async_socket,
        tx: pdu_tx,
        rx: pdu_rx,
    };

    Ok(task)
}

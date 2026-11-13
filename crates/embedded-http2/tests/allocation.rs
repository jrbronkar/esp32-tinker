//! Isolated binary so allocator fault injection cannot affect other tests.

use embedded_http2::{Error, Server};
use embedded_io_adapters::tokio_1::FromTokio;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};
use tokio::io::{AsyncWriteExt, duplex};
use zerodds_http2::CLIENT_PREFACE;

const PAYLOAD: usize = 1234;
const FRAME: usize = PAYLOAD + 9;
static DENY_FRAME: AtomicBool = AtomicBool::new(false);

struct RejectFrame;

// SAFETY: Accepted allocations and deallocations use the same system allocator
// and layout. Returning null signals failure to the caller.
unsafe impl GlobalAlloc for RejectFrame {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() == FRAME && DENY_FRAME.swap(false, Ordering::Relaxed) {
            return ptr::null_mut();
        }
        // SAFETY: The caller supplied the allocation layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: The caller supplied a live allocation and its original layout.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: RejectFrame = RejectFrame;

#[tokio::test]
async fn failed_frame_allocation_closes_only_its_connection()
-> Result<(), Box<dyn std::error::Error>> {
    for deny in [true, false] {
        let (socket, mut peer) = duplex(4096);
        peer.write_all(CLIENT_PREFACE).await?;
        peer.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0]).await?;
        // An extension frame with an otherwise valid header and payload length.
        let mut frame = [0; FRAME];
        frame[1..3].copy_from_slice(&(PAYLOAD as u16).to_be_bytes());
        frame[3] = 0xff;
        peer.write_all(&frame).await?;

        let mut server = Server::handshake(FromTokio::new(socket))
            .await
            .map_err(|error| format!("{error:?}"))?;
        DENY_FRAME.store(deny, Ordering::Relaxed);
        let result = server.next_event().await;
        assert!(!DENY_FRAME.load(Ordering::Relaxed));
        if deny {
            assert!(matches!(result, Err(Error::ResourceExhausted)));
        } else {
            assert!(matches!(result, Ok(embedded_http2::Event::Control)));
        }
    }
    Ok(())
}

//! Isolated binary so allocator fault injection cannot affect other tests.

use embedded_http2::{Error, Server};
use embedded_io_adapters::tokio_1::FromTokio;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    ptr,
};
use tokio::io::{AsyncWriteExt, duplex};
use zerodds_http2::CLIENT_PREFACE;

const PAYLOAD: usize = 1234;
const FRAME: usize = PAYLOAD + 9;
thread_local! {
    static DENIED_SIZE: Cell<Option<usize>> = const { Cell::new(None) };
}

struct FaultAllocator;

// SAFETY: Accepted allocations and deallocations use the same system allocator
// and layout. Returning null signals failure to the caller.
unsafe impl GlobalAlloc for FaultAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if DENIED_SIZE.with(|denied| {
            if denied.get() == Some(layout.size()) {
                denied.take();
                true
            } else {
                false
            }
        }) {
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
static ALLOCATOR: FaultAllocator = FaultAllocator;

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
        DENIED_SIZE.set(deny.then_some(FRAME));
        let result = server.next_event().await;
        assert_eq!(DENIED_SIZE.take(), None);
        if deny {
            assert!(matches!(result, Err(Error::ResourceExhausted)));
        } else {
            assert!(matches!(result, Ok(embedded_http2::Event::Control)));
        }
    }
    Ok(())
}

#[tokio::test]
async fn failed_response_data_allocation_keeps_sibling_usable()
-> Result<(), Box<dyn std::error::Error>> {
    use embedded_http2::{HeaderField, server::StreamError};
    use tokio::io::AsyncReadExt;

    const REQUEST: [u8; 6] = [0x82, 0x86, 0x84, 0x01, 0x01, b'a'];
    let (socket, mut peer) = duplex(4096);
    let server =
        embedded_http2::service::serve_streams(FromTokio::new(socket), |request| async move {
            let headers = vec![HeaderField {
                name: ":status".into(),
                value: "200".into(),
            }];
            if request.head.stream_id == 1 {
                let mut send = request.respond.send_response(headers).unwrap();
                DENIED_SIZE.set(Some(PAYLOAD));
                let result = send.send_data(&[0; PAYLOAD]).await;
                assert_eq!(DENIED_SIZE.take(), None);
                assert!(matches!(result, Err(StreamError::ResourceExhausted)));
                // Dropping the send stream resets this stream.
            } else {
                request.respond.finish_headers(headers).await.unwrap();
            }
            Ok(())
        });
    let client = async move {
        peer.write_all(CLIENT_PREFACE).await?;
        peer.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0]).await?;
        for id in [1, 3] {
            peer.write_all(&[0, 0, REQUEST.len() as u8, 1, 5, 0, 0, 0, id])
                .await?;
            peer.write_all(&REQUEST).await?;
        }
        let mut reset = false;
        let mut sibling_finished = false;
        while !reset || !sibling_finished {
            let mut header = [0; 9];
            peer.read_exact(&mut header).await?;
            let size = u32::from_be_bytes([0, header[0], header[1], header[2]]) as usize;
            let mut payload = vec![0; size];
            peer.read_exact(&mut payload).await?;
            match (header[3], header[8]) {
                (3, 1) => reset = true,
                (1, 3) => sibling_finished = header[4] & 1 != 0,
                _ => {}
            }
        }
        Ok::<_, std::io::Error>(())
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        // Drop the peer when the two responses have been observed.
        let (_, result) = tokio::join!(server, client);
        result
    })
    .await??;
    Ok(())
}

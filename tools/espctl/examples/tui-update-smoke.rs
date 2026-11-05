//! Local gRPC fixture for the Python pseudo-terminal tests.
#[path = "../tests/support/ota.rs"]
mod ota;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let image = args.next().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "expected image output path",
        )
    })?;
    let fault = match args.next().as_deref().and_then(std::ffi::OsStr::to_str) {
        None => "",
        Some("lost-commit") => "lost-commit",
        Some(_) => return Err(std::io::Error::other("unknown mock fault")),
    };
    std::fs::write(image, ota::image())?;
    tokio::task::LocalSet::new()
        .run_until(async {
            let (address, server, _) = ota::start(fault, true).await;
            println!("{}", address.port());
            server.await.map_err(std::io::Error::other)
        })
        .await
}

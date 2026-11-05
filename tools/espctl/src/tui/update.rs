use std::{
    fs,
    io::{self, Read},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use ota_protocol::{ReadLogsResponse, StatusResponse};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

use crate::upload::{self, PreparedImage, Progress};

struct Board {
    address: SocketAddr,
    id: String,
    version: String,
}

struct Verified {
    board: Board,
    image: PreparedImage,
    slot: u32,
}

enum Outcome {
    Verified(Verified),
    Activated(String),
}

type LogUpdate = Result<ReadLogsResponse, String>;

struct Job {
    task: JoinHandle<io::Result<Outcome>>,
    progress: watch::Receiver<Option<Progress>>,
    initial: &'static str,
    logs: mpsc::Receiver<LogUpdate>,
}

type ImageSource = (PathBuf, Option<String>);

struct Picker {
    images: Vec<(PathBuf, String)>,
    selected: usize,
}

fn local_images(directory: &Path) -> io::Result<Picker> {
    let mut images = Vec::new();
    for entry in fs::read_dir(directory)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_file())
            || path.extension().is_none_or(|ext| ext != "bin")
        {
            continue;
        }
        let mut header = [0; ota_core::IMAGE_HEADER_SIZE];
        let Ok(mut file) = fs::File::open(&path) else {
            continue;
        };
        if file.read_exact(&mut header).is_ok() {
            if let Ok(version) = ota_core::image_version(&header) {
                images.push((path, version.to_owned()));
            }
        }
    }
    images.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(Picker {
        images,
        selected: 0,
    })
}

#[derive(Default)]
pub(super) struct Workflow {
    config: Option<ImageSource>,
    picker: Option<Picker>,
    verified: Option<Verified>,
    confirming: bool,
    job: Option<Job>,
    notice: String,
}

impl Workflow {
    pub(super) fn new(config: Option<ImageSource>) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.config.is_some()
            || self.picking()
            || self.busy()
            || self.verified.is_some()
            || !self.notice.is_empty()
    }

    pub(super) fn picking(&self) -> bool {
        self.picker.is_some()
    }

    pub(super) fn previous_image(&mut self) {
        if let Some(picker) = &mut self.picker {
            picker.selected = picker.selected.saturating_sub(1);
        }
    }

    pub(super) fn next_image(&mut self) {
        if let Some(picker) = &mut self.picker {
            picker.selected = (picker.selected + 1).min(picker.images.len() - 1);
        }
    }

    pub(super) fn cancel_picker(&mut self) {
        self.picker = None;
    }

    pub(super) fn busy(&self) -> bool {
        self.job.is_some()
    }

    pub(super) fn confirming(&self) -> bool {
        self.confirming
    }

    pub(super) fn clear(&mut self) {
        self.picker = None;
        self.verified = None;
        self.confirming = false;
        self.notice.clear();
    }

    pub(super) fn verify(
        &mut self,
        address: Option<SocketAddr>,
        status: Option<&StatusResponse>,
        timeout: Duration,
    ) {
        if let Some(source) = self.config.clone() {
            self.start_verify(source, address, status, timeout);
            return;
        }
        self.notice.clear();
        match local_images(Path::new("target/ota")) {
            Ok(picker) if !picker.images.is_empty() => self.picker = Some(picker),
            Ok(_) => {
                self.notice = "No compatible images in target/ota. Run nix run .#ota-image.".into()
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.notice = "No target/ota directory. Run nix run .#ota-image.".into();
            }
            Err(error) => self.notice = format!("Could not scan target/ota: {error}"),
        }
    }

    pub(super) fn verify_selected(
        &mut self,
        address: Option<SocketAddr>,
        status: Option<&StatusResponse>,
        timeout: Duration,
    ) {
        if let Some(picker) = self.picker.take() {
            if let Some((path, version)) = picker.images.into_iter().nth(picker.selected) {
                self.start_verify((path, Some(version)), address, status, timeout);
            }
        }
    }

    fn start_verify(
        &mut self,
        (path, version): ImageSource,
        address: Option<SocketAddr>,
        status: Option<&StatusResponse>,
        timeout: Duration,
    ) {
        let Some((address, status)) = address.zip(status).filter(|(_, s)| s.ota_available) else {
            self.notice = "Select a connected board with OTA available.".into();
            return;
        };
        let id = status.device_id.to_string();
        self.clear();
        self.job = Some(spawn(
            "Validating application image",
            move |progress, on_log| {
                let image = PreparedImage::read(&path, version.as_deref())?;
                let board = Board {
                    address,
                    id,
                    version: image.version().to_owned(),
                };
                let result = upload::update_board(
                    board.address,
                    timeout,
                    &image,
                    &board.id,
                    None,
                    progress,
                    on_log,
                )?;
                Ok(Outcome::Verified(Verified {
                    board,
                    image,
                    slot: result.target_offset,
                }))
            },
        ));
    }

    pub(super) fn ask_activation(&mut self) {
        if self.verified.is_some() {
            self.confirming = true;
        }
    }

    pub(super) fn cancel_confirmation(&mut self) {
        self.confirming = false;
    }

    pub(super) fn activate(&mut self, timeout: Duration) {
        if !std::mem::take(&mut self.confirming) {
            return;
        }
        let Some(verified) = self.verified.take() else {
            return;
        };
        self.job = Some(spawn(
            "Preparing verified image for activation",
            move |progress, on_log| {
                let Verified { board, image, slot } = verified;
                let mut commit_started = false;
                let result = upload::update_board(
                    board.address,
                    timeout,
                    &image,
                    &board.id,
                    Some(slot),
                    &mut |event| {
                        if matches!(event, Progress::Committing) {
                            commit_started = true;
                        }
                        progress(event);
                    },
                    on_log,
                );
                if !commit_started {
                    return result
                        .and_then(|_| Err(io::Error::other("activation was not attempted")));
                }
                // A missing ACK can still mean the board rebooted. Only read status here.
                progress(Progress::CheckingBoot);
                if running_expected_image(&board, slot, timeout) {
                    return Ok(Outcome::Activated(format!(
                        "{} is running {} from slot 0x{slot:x}.",
                        board.id, board.version
                    )));
                }
                let message = match result {
                Ok(_) => "Activation acknowledged. The new firmware and slot could not be confirmed.",
                Err(_) => "Activation outcome unknown. The new firmware and slot could not be confirmed.",
            };
                Err(io::Error::other(message))
            },
        ));
    }

    pub(super) fn next_log(&mut self) -> Option<LogUpdate> {
        self.job.as_mut()?.logs.try_recv().ok()
    }

    pub(super) async fn poll(&mut self) -> bool {
        if self
            .job
            .as_ref()
            .is_some_and(|job| job.task.is_finished() && job.logs.is_empty())
        {
            if let Some(job) = self.job.take() {
                match job.task.await {
                    Ok(Ok(Outcome::Verified(verified))) => self.verified = Some(verified),
                    Ok(Ok(Outcome::Activated(message))) => self.notice = message,
                    Ok(Err(error)) => self.notice = error.to_string(),
                    Err(error) => self.notice = format!("Update task ended: {error}"),
                }
                return true;
            }
        }
        if let Some(job) = &mut self.job {
            if job.progress.has_changed().unwrap_or(false) {
                job.progress.borrow_and_update();
                return true;
            }
        }
        false
    }

    pub(super) fn text(&self) -> String {
        if let Some(job) = &self.job {
            return match *job.progress.borrow() {
                None => job.initial.into(),
                Some(Progress::Writing { written, total }) => {
                    format!("Writing image · {written}/{total} bytes")
                }
                Some(Progress::Verifying) => "Verifying image on board".into(),
                Some(Progress::Committing) => "Activating image".into(),
                Some(Progress::CheckingBoot) => "Checking running firmware and slot".into(),
            };
        }
        if let Some(picker) = &self.picker {
            let (path, version) = &picker.images[picker.selected];
            return format!(
                "Image {}/{} · {} · {}\n↑/↓ Choose   Enter Upload and verify   Esc Cancel",
                picker.selected + 1,
                picker.images.len(),
                version.escape_debug(),
                path.display().to_string().escape_debug(),
            );
        }
        if let Some(verified) = &self.verified {
            let board = &verified.board;
            let id = board.id.escape_debug();
            let version = board.version.escape_debug();
            return if self.confirming {
                format!(
                    "Activate {} on {} ({}) and reboot?\nY Resend, verify, and activate   Esc Cancel",
                    version, id, board.address
                )
            } else {
                format!(
                    "Verified {} on {} · slot 0x{:x}\nA Review activation   U Verify again",
                    version, id, verified.slot
                )
            };
        }
        if !self.notice.is_empty() {
            return self.notice.escape_debug().to_string();
        }
        if let Some((path, _)) = &self.config {
            return format!(
                "U Upload and verify {}",
                path.display().to_string().escape_debug()
            );
        }
        String::new()
    }
}

fn spawn(
    initial: &'static str,
    work: impl FnOnce(&mut dyn FnMut(Progress), &mut dyn FnMut(LogUpdate)) -> io::Result<Outcome>
        + Send
        + 'static,
) -> Job {
    let (sender, progress) = watch::channel(None);
    let (log_sender, logs) = mpsc::channel(32);
    let task = tokio::task::spawn_blocking(move || {
        work(
            &mut |event| {
                sender.send_replace(Some(event));
            },
            &mut |batch| {
                let _ = log_sender.try_send(batch);
            },
        )
    });
    Job {
        task,
        progress,
        initial,
        logs,
    }
}

fn running_expected_image(board: &Board, slot: u32, timeout: Duration) -> bool {
    let end = Instant::now() + Duration::from_secs(25);
    loop {
        let Some(remaining) = end.checked_duration_since(Instant::now()) else {
            return false;
        };
        let query = crate::query_status_with_options(
            board.address,
            timeout.min(remaining).min(Duration::from_secs(2)),
            false,
            true,
        );
        if let Ok(query) = query {
            let status = query.status;
            if status.device_id.as_str() != board.id {
                return false;
            }
            if status.firmware_version.as_str() == board.version
                && status
                    .boot()
                    .and_then(|boot| boot.running())
                    .is_some_and(|running| running.offset == slot)
            {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
#[path = "../../tests/support/ota.rs"]
mod mock_board;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_reads_embedded_versions_and_ignores_incompatible_files() {
        let directory = std::env::temp_dir().join(format!("espctl-picker-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let expected = directory.join("custom-name.bin");
        fs::write(&expected, mock_board::image()).unwrap();
        fs::write(directory.join("bad.bin"), b"invalid image").unwrap();
        fs::write(directory.join("not-an-export"), mock_board::image()).unwrap();
        let picker = local_images(&directory).unwrap();
        fs::remove_dir_all(directory).unwrap();
        assert_eq!(picker.images, [(expected, "v1".into())]);
    }

    async fn finish(workflow: &mut Workflow) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while workflow.busy() {
                while workflow.next_log().is_some() {}
                workflow.poll().await;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dashboard update timed out");
    }

    #[tokio::test]
    async fn update_requires_confirmation_and_checks_boot_over_grpc() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let timeout = Duration::from_secs(2);
                for fault in ["", "lost-commit"] {
                    let (address, server, board) = mock_board::start(fault, true).await;
                    let path = std::env::temp_dir().join(format!(
                        "espctl-dashboard-{}-{fault}.bin",
                        std::process::id()
                    ));
                    std::fs::write(&path, mock_board::image()).unwrap();
                    let mut workflow = Workflow::new(Some((path.clone(), None)));
                    let status =
                        tokio::task::spawn_blocking(move || crate::query_status(address, timeout))
                            .await
                            .unwrap()
                            .unwrap();
                    workflow.verify(Some(address), Some(&status), timeout);
                    finish(&mut workflow).await;
                    std::fs::remove_file(path).unwrap();
                    assert!(
                        workflow.text().contains("Verified v1"),
                        "{}",
                        workflow.text()
                    );
                    assert_eq!(board.writes.get(), 1);
                    assert_eq!(board.commits.get(), 0);
                    assert!(!board.pending.get());
                    workflow.activate(timeout);
                    assert!(!workflow.busy());
                    workflow.ask_activation();
                    assert!(workflow.confirming());
                    workflow.cancel_confirmation();
                    workflow.activate(timeout);
                    assert!(!workflow.busy());

                    workflow.ask_activation();
                    workflow.activate(timeout);
                    finish(&mut workflow).await;
                    assert!(
                        workflow.text().contains("running v1 from slot 0x310000"),
                        "{}",
                        workflow.text()
                    );
                    assert_eq!(board.commits.get(), 1);
                    assert!(board.pending.get());
                    assert_eq!(
                        *board.requests.borrow(),
                        [
                            "begin", "chunk", "finish", "abort", "begin", "chunk", "finish",
                            "commit"
                        ]
                    );
                    server.abort();
                    let _ = server.await;
                }
            })
            .await;
    }
}

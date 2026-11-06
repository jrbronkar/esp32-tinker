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
    application: String,
    version: String,
}

struct PendingUpdate {
    board: Board,
    image: PreparedImage,
}

type LogUpdate = Result<ReadLogsResponse, String>;

struct Job {
    task: JoinHandle<io::Result<String>>,
    progress: watch::Receiver<Option<Progress>>,
    initial: &'static str,
    logs: mpsc::Receiver<LogUpdate>,
}

type ImageSource = (PathBuf, Option<String>);

const IMAGE_SCAN_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Picker {
    images: Vec<(PathBuf, String, String)>,
    selected: usize,
    last_scan: Option<Instant>,
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
            if let (Ok(version), Ok(application)) = (
                ota_core::image_version(&header),
                ota_core::image_application(&header),
            ) {
                images.push((path, version.to_owned(), application.to_owned()));
            }
        }
    }
    images.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(Picker {
        images,
        last_scan: Some(Instant::now()),
        ..Picker::default()
    })
}

#[derive(Default)]
pub(super) struct Workflow {
    config: Option<ImageSource>,
    picker: Option<Picker>,
    pending: Option<PendingUpdate>,
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
            || self.pending.is_some()
            || !self.notice.is_empty()
    }

    pub(super) fn picking(&self) -> bool {
        self.picker.is_some()
    }

    pub(super) fn images(&self) -> Option<(&[(PathBuf, String, String)], usize)> {
        self.picker
            .as_ref()
            .map(|picker| (picker.images.as_slice(), picker.selected))
    }

    pub(super) fn write_progress(&self) -> Option<(u32, u32)> {
        match *self.job.as_ref()?.progress.borrow() {
            Some(Progress::Writing { written, total }) => Some((written, total)),
            _ => None,
        }
    }

    pub(super) fn previous_image(&mut self) {
        if let Some(picker) = &mut self.picker {
            picker.selected = picker.selected.saturating_sub(1);
        }
    }

    pub(super) fn next_image(&mut self) {
        if let Some(picker) = &mut self.picker {
            picker.selected = (picker.selected + 1).min(picker.images.len().saturating_sub(1));
        }
    }

    pub(super) fn cancel_picker(&mut self) {
        self.picker = None;
    }

    pub(super) fn busy(&self) -> bool {
        self.job.is_some()
    }

    pub(super) fn confirming(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn clear(&mut self) {
        self.picker = None;
        self.pending = None;
        self.notice.clear();
    }

    pub(super) fn choose_image(
        &mut self,
        address: Option<SocketAddr>,
        status: Option<&StatusResponse>,
    ) {
        if let Some(source) = self.config.clone() {
            self.prepare(source, address, status);
            return;
        }
        self.notice.clear();
        self.picker = Some(Picker::default());
        self.refresh_images();
    }

    fn refresh_images(&mut self) -> bool {
        let Some(picker) = &mut self.picker else {
            return false;
        };
        if picker
            .last_scan
            .is_some_and(|scan| scan.elapsed() < IMAGE_SCAN_INTERVAL)
        {
            return false;
        }
        picker.last_scan = Some(Instant::now());
        let images = local_images(Path::new("target/ota"))
            .map(|scan| scan.images)
            .unwrap_or_default();
        if images == picker.images {
            return false;
        }
        picker.selected = picker
            .images
            .get(picker.selected)
            .and_then(|(selected, _, _)| images.iter().position(|(path, _, _)| path == selected))
            .unwrap_or(0);
        picker.images = images;
        true
    }

    pub(super) fn choose_selected(
        &mut self,
        address: Option<SocketAddr>,
        status: Option<&StatusResponse>,
    ) {
        if self
            .picker
            .as_ref()
            .is_some_and(|picker| picker.images.is_empty())
        {
            return;
        }
        if let Some(picker) = self.picker.take() {
            if let Some((path, version, _)) = picker.images.into_iter().nth(picker.selected) {
                self.prepare((path, Some(version)), address, status);
            }
        }
    }

    fn prepare(
        &mut self,
        (path, version): ImageSource,
        address: Option<SocketAddr>,
        status: Option<&StatusResponse>,
    ) {
        let Some((address, status)) = address.zip(status).filter(|(_, s)| s.ota_available) else {
            self.notice = "Select a connected board with OTA available.".into();
            return;
        };
        let id = status.device_id.to_string();
        self.clear();
        match PreparedImage::read(&path, version.as_deref()) {
            Ok(image) => {
                // A device-demo migration build can explicitly accept imu-demo.
                if image.application() != status.application.as_str()
                    && !(status.application.as_str() == ota_core::APP
                        && image.application() == "imu-demo")
                {
                    self.notice = format!(
                        "Image is for {} · board runs {}",
                        image.application(),
                        status.application
                    );
                    return;
                }
                let board = Board {
                    address,
                    id,
                    application: image.application().to_owned(),
                    version: image.version().to_owned(),
                };
                self.pending = Some(PendingUpdate { board, image });
            }
            Err(error) => self.notice = error.to_string(),
        }
    }

    pub(super) fn cancel_confirmation(&mut self) {
        self.pending = None;
    }

    pub(super) fn activate(&mut self, timeout: Duration) {
        let Some(PendingUpdate { board, image }) = self.pending.take() else {
            return;
        };
        self.job = Some(spawn(
            "Connecting to upload image",
            move |progress, on_log| {
                let mut commit_slot = None;
                let result = upload::update_board(
                    board.address,
                    timeout,
                    &image,
                    &board.id,
                    &mut |event| {
                        if let Progress::Committing { target_offset } = event {
                            commit_slot = Some(target_offset);
                        }
                        progress(event);
                    },
                    on_log,
                );
                let Some(slot) = commit_slot else {
                    return result
                        .and_then(|_| Err(io::Error::other("activation was not attempted")));
                };
                // A missing ACK can still mean the board rebooted. Only read status here.
                progress(Progress::CheckingBoot);
                if running_expected_image(&board, slot, timeout) {
                    return Ok(format!(
                        "{} is running {} from slot 0x{slot:x}.",
                        board.id, board.version
                    ));
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
        let images_changed = self.refresh_images();
        if self
            .job
            .as_ref()
            .is_some_and(|job| job.task.is_finished() && job.logs.is_empty())
        {
            if let Some(job) = self.job.take() {
                match job.task.await {
                    Ok(Ok(message)) => self.notice = message,
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
        images_changed
    }

    pub(super) fn text(&self) -> String {
        if let Some(job) = &self.job {
            return match *job.progress.borrow() {
                None => job.initial.into(),
                Some(Progress::Writing { written, total }) => {
                    format!("Writing image · {written}/{total} bytes")
                }
                Some(Progress::Verifying) => "Verifying image on board".into(),
                Some(Progress::Committing { .. }) => "Activating image".into(),
                Some(Progress::CheckingBoot) => "Checking running firmware and slot".into(),
            };
        }
        if let Some(pending) = &self.pending {
            let board = &pending.board;
            return format!(
                "Update {} ({}) to {} {}?\nThe board will reboot after the image is uploaded and verified.",
                board.id.escape_debug(), board.address, board.application.escape_debug(), board.version.escape_debug(),
            );
        }
        if !self.notice.is_empty() {
            return self.notice.escape_debug().to_string();
        }
        if let Some((path, _)) = &self.config {
            return format!(
                "U Review update for {}",
                path.display().to_string().escape_debug()
            );
        }
        String::new()
    }
}

fn spawn(
    initial: &'static str,
    work: impl FnOnce(&mut dyn FnMut(Progress), &mut dyn FnMut(LogUpdate)) -> io::Result<String>
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
            if status.application.as_str() == board.application
                && status.firmware_version.as_str() == board.version
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
        assert_eq!(
            picker.images,
            [(expected, "v1".into(), ota_core::APP.into())]
        );
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
                    workflow.choose_image(Some(address), Some(&status));
                    assert!(workflow.confirming());
                    assert!(!workflow.busy());
                    assert_eq!(board.writes.get(), 0);
                    assert_eq!(board.commits.get(), 0);
                    workflow.cancel_confirmation();
                    workflow.activate(timeout);
                    assert!(!workflow.busy());

                    workflow.choose_image(Some(address), Some(&status));
                    std::fs::remove_file(path).unwrap();
                    workflow.activate(timeout);
                    finish(&mut workflow).await;
                    assert!(
                        workflow.text().contains("running v1 from slot 0x310000"),
                        "{}",
                        workflow.text()
                    );
                    assert_eq!(board.writes.get(), 1);
                    assert_eq!(board.commits.get(), 1);
                    assert!(board.pending.get());
                    assert_eq!(
                        *board.requests.borrow(),
                        ["begin", "chunk", "finish", "commit"]
                    );
                    server.abort();
                    let _ = server.await;
                }
            })
            .await;
    }
}

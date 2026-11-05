use std::{
    io,
    net::SocketAddr,
    path::PathBuf,
    time::{Duration, Instant},
};

use ota_protocol::StatusResponse;
use tokio::{sync::watch, task::JoinHandle};

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

struct Job {
    task: JoinHandle<io::Result<Outcome>>,
    progress: watch::Receiver<Option<Progress>>,
    initial: &'static str,
}

#[derive(Default)]
pub(super) struct Workflow {
    config: Option<(PathBuf, String)>,
    verified: Option<Verified>,
    confirming: bool,
    job: Option<Job>,
    notice: String,
}

impl Workflow {
    pub(super) fn new(config: Option<(PathBuf, String)>) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.config.is_some()
    }

    pub(super) fn busy(&self) -> bool {
        self.job.is_some()
    }

    pub(super) fn confirming(&self) -> bool {
        self.confirming
    }

    pub(super) fn clear(&mut self) {
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
        let Some((path, version)) = self.config.clone() else {
            return;
        };
        let Some((address, status)) = address.zip(status).filter(|(_, s)| s.ota_available) else {
            self.notice = "Select a connected board with OTA available.".into();
            return;
        };
        let board = Board {
            address,
            id: status.device_id.to_string(),
            version: version.clone(),
        };
        self.clear();
        self.job = Some(spawn("Validating application image", move |progress| {
            let image = PreparedImage::read(&path, &version)?;
            let result =
                upload::update_board(board.address, timeout, &image, &board.id, None, progress)?;
            Ok(Outcome::Verified(Verified {
                board,
                image,
                slot: result.target_offset,
            }))
        }));
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
        self.confirming = false;
        let Some(verified) = self.verified.take() else {
            return;
        };
        self.job = Some(spawn(
            "Preparing verified image for activation",
            move |progress| {
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

    pub(super) async fn poll(&mut self) -> bool {
        if self.job.as_ref().is_some_and(|job| job.task.is_finished()) {
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
        if let Some((path, version)) = &self.config {
            return format!(
                "U Upload and verify {} ({})",
                version.escape_debug(),
                path.display().to_string().escape_debug()
            );
        }
        String::new()
    }
}

fn spawn(
    initial: &'static str,
    work: impl FnOnce(&mut dyn FnMut(Progress)) -> io::Result<Outcome> + Send + 'static,
) -> Job {
    let (sender, progress) = watch::channel(None);
    let task = tokio::task::spawn_blocking(move || {
        work(&mut |event| {
            sender.send_replace(Some(event));
        })
    });
    Job {
        task,
        progress,
        initial,
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

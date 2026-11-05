use std::{
    io,
    net::SocketAddr,
    path::PathBuf,
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
        self.job = Some(spawn(
            "Validating application image",
            move |progress, on_log| {
                let image = PreparedImage::read(&path, &version)?;
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
                    let mut workflow = Workflow::new(Some((path.clone(), "v1".into())));
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

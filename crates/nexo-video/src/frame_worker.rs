use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};

use crate::{VideoError, VideoFrame};

/// Reads a platform capture source away from the UI/runtime thread and keeps
/// only its newest result. Capture APIs are allowed to block while waiting for
/// a sample, but that must never block call controls or network processing.
pub(crate) struct FrameWorker {
    latest: Arc<Mutex<Option<Result<VideoFrame, VideoError>>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl FrameWorker {
    pub(crate) fn spawn_open<S, O, R, F>(
        open: O,
        resolution: R,
        mut read: F,
    ) -> Result<(Self, (u32, u32)), VideoError>
    where
        O: FnOnce() -> Result<S, VideoError> + Send + 'static,
        R: Fn(&S) -> (u32, u32) + Send + 'static,
        F: FnMut(&mut S) -> Result<Option<VideoFrame>, VideoError> + Send + 'static,
    {
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_latest = Arc::clone(&latest);
        let thread_stop = Arc::clone(&stop);
        let (startup_sender, startup_receiver) = std::sync::mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("nexo-video-capture".to_owned())
            .spawn(move || {
                let mut source = match open() {
                    Ok(source) => source,
                    Err(error) => {
                        let _ = startup_sender.send(Err(error));
                        return;
                    }
                };
                let negotiated_resolution = resolution(&source);
                if startup_sender.send(Ok(negotiated_resolution)).is_err() {
                    return;
                }
                while !thread_stop.load(Ordering::Acquire) {
                    match read(&mut source) {
                        Ok(Some(frame)) => {
                            if let Ok(mut latest) = thread_latest.lock() {
                                *latest = Some(Ok(frame));
                            } else {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(error) => {
                            if let Ok(mut latest) = thread_latest.lock() {
                                *latest = Some(Err(error));
                            }
                            break;
                        }
                    }
                }
            })
            .map_err(|error| VideoError::platform(format!("thread de captura: {error}")))?;

        let negotiated_resolution = startup_receiver.recv().map_err(|_| {
            VideoError::platform("thread de captura encerrou durante a inicializacao")
        })??;

        Ok((
            Self {
                latest,
                stop,
                worker: Some(worker),
            },
            negotiated_resolution,
        ))
    }

    pub(crate) fn take_latest(&mut self) -> Option<Result<VideoFrame, VideoError>> {
        self.latest.lock().ok()?.take()
    }
}

impl Drop for FrameWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // A platform read can be blocked in native code. Dropping the handle
        // detaches that reader after the stop flag is set rather than blocking
        // the call thread during teardown.
        let _ = self.worker.take();
    }
}

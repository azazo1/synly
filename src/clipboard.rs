use anyhow::{Context, Result, anyhow};
use clipboard_rs::{
    Clipboard, ClipboardContext, ClipboardHandler, ClipboardWatcher, ClipboardWatcherContext,
    WatcherShutdown,
};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use std::thread;
use tokio::sync::mpsc;

#[derive(Clone, Default)]
pub struct ClipboardSync {
    state: Arc<Mutex<ClipboardSyncState>>,
}

pub struct ClipboardWatcherHandle {
    shutdown: Option<WatcherShutdown>,
    thread: Option<thread::JoinHandle<()>>,
}

#[derive(Default)]
struct ClipboardSyncState {
    last_sent_signature: Option<String>,
    suppress_next_signature: Option<String>,
}

struct LocalClipboardHandler {
    ctx: ClipboardContext,
    state: Arc<Mutex<ClipboardSyncState>>,
    tx: mpsc::UnboundedSender<String>,
}

impl ClipboardSync {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start_local_watcher(
        &self,
        tx: mpsc::UnboundedSender<String>,
    ) -> Result<ClipboardWatcherHandle> {
        let handler = LocalClipboardHandler::new(self.state.clone(), tx)?;
        let mut watcher: ClipboardWatcherContext<LocalClipboardHandler> =
            ClipboardWatcherContext::new().map_err(clipboard_error)?;
        let shutdown = watcher.add_handler(handler).get_shutdown_channel();
        let thread = thread::Builder::new()
            .name("synly-clipboard-watch".to_string())
            .spawn(move || watcher.start_watch())
            .context("failed to spawn clipboard watcher thread")?;

        Ok(ClipboardWatcherHandle {
            shutdown: Some(shutdown),
            thread: Some(thread),
        })
    }

    pub async fn publish_initial_text(&self, tx: &mpsc::UnboundedSender<String>) -> Result<()> {
        let Some(text) = read_clipboard_text().await? else {
            return Ok(());
        };

        if self.note_local_text(&text)? {
            let _ = tx.send(text);
        }
        Ok(())
    }

    pub async fn apply_remote_text(&self, text: String) -> Result<()> {
        let signature = text_signature(&text);
        let state = self.state.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let ctx = ClipboardContext::new().map_err(clipboard_error)?;
            let already_matches = ctx.get_text().ok().as_deref() == Some(text.as_str());
            if !already_matches {
                ctx.set_text(text).map_err(clipboard_error)?;
            }

            let mut guard = state
                .lock()
                .map_err(|_| anyhow!("clipboard sync state poisoned"))?;
            guard.note_remote_text(signature, !already_matches);
            Ok(())
        })
        .await
        .context("clipboard apply task failed")??;

        Ok(())
    }

    fn note_local_text(&self, text: &str) -> Result<bool> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| anyhow!("clipboard sync state poisoned"))?;
        Ok(guard.note_local_text(text))
    }
}

impl ClipboardWatcherHandle {
    pub fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.stop();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl ClipboardSyncState {
    fn note_local_text(&mut self, text: &str) -> bool {
        let signature = text_signature(text);
        if self.suppress_next_signature.as_ref() == Some(&signature) {
            self.suppress_next_signature = None;
            self.last_sent_signature = Some(signature);
            return false;
        }

        if self.last_sent_signature.as_ref() == Some(&signature) {
            return false;
        }

        self.last_sent_signature = Some(signature);
        true
    }

    fn note_remote_text(&mut self, signature: String, applied_to_local_clipboard: bool) {
        self.last_sent_signature = Some(signature.clone());
        self.suppress_next_signature = applied_to_local_clipboard.then_some(signature);
    }
}

impl LocalClipboardHandler {
    fn new(
        state: Arc<Mutex<ClipboardSyncState>>,
        tx: mpsc::UnboundedSender<String>,
    ) -> Result<Self> {
        let ctx = ClipboardContext::new().map_err(clipboard_error)?;
        Ok(Self { ctx, state, tx })
    }
}

impl ClipboardHandler for LocalClipboardHandler {
    fn on_clipboard_change(&mut self) {
        let Ok(text) = self.ctx.get_text() else {
            return;
        };

        let should_send = match self.state.lock() {
            Ok(mut state) => state.note_local_text(&text),
            Err(_) => false,
        };
        if should_send {
            let _ = self.tx.send(text);
        }
    }
}

async fn read_clipboard_text() -> Result<Option<String>> {
    tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let ctx = ClipboardContext::new().map_err(clipboard_error)?;
        Ok(ctx.get_text().ok())
    })
    .await
    .context("clipboard read task failed")?
}

fn clipboard_error(err: Box<dyn std::error::Error + Send + Sync + 'static>) -> anyhow::Error {
    anyhow!(err.to_string())
}

fn text_signature(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::{ClipboardSyncState, text_signature};

    #[test]
    fn remote_apply_is_suppressed_once() {
        let mut state = ClipboardSyncState::default();
        state.note_remote_text(text_signature("hello"), true);

        assert!(!state.note_local_text("hello"));
        assert!(!state.note_local_text("hello"));
    }

    #[test]
    fn remote_apply_without_local_change_does_not_block_future_same_text() {
        let mut state = ClipboardSyncState::default();
        state.note_remote_text(text_signature("hello"), false);

        assert!(!state.note_local_text("hello"));

        state.note_remote_text(text_signature("world"), false);
        assert!(state.note_local_text("hello"));
    }

    #[test]
    fn different_local_text_after_remote_apply_is_sent() {
        let mut state = ClipboardSyncState::default();
        state.note_remote_text(text_signature("hello"), true);

        assert!(state.note_local_text("world"));
    }
}

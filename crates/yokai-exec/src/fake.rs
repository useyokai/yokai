use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::Instant;
use yokai_types::{
    AssistantResponse, FakeFailure, FakeOutcome, MAX_CHUNK_BYTES, MAX_RESPONSE_BYTES, MessageId,
    SubmitMessage,
};

#[derive(Clone, Default)]
pub(crate) struct FakeModel {
    #[cfg(feature = "test-api")]
    pub test: Option<TestFake>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub index: usize,
    pub text: String,
}

impl FakeModel {
    pub(crate) fn run(
        &self,
        request: &SubmitMessage,
        assistant_id: MessageId,
        progress: &SyncSender<Chunk>,
        deadline: Instant,
    ) -> Option<FakeOutcome> {
        #[cfg(feature = "test-api")]
        if let Some(test) = &self.test {
            if let Some(pause) = &test.pause {
                if !pause.run_before(deadline) {
                    return None;
                }
            }
            test.invocations.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        #[cfg(not(feature = "test-api"))]
        let _ = deadline;
        Some(self.produce(request, assistant_id, progress).unwrap_or_else(FakeOutcome::Failed))
    }

    fn produce(
        &self,
        request: &SubmitMessage,
        assistant_id: MessageId,
        progress: &SyncSender<Chunk>,
    ) -> Result<FakeOutcome, FakeFailure> {
        const PREFIX: &str = "fake: ";
        let mut text = String::new();
        #[cfg(feature = "test-api")]
        if let Some(reply) = self.test.as_ref().and_then(|test| test.reply.as_deref()) {
            text.push_str(reply);
        }
        if text.is_empty() {
            let bytes = PREFIX.len() + request.text().len();
            if bytes > MAX_RESPONSE_BYTES {
                return Err(FakeFailure::InvalidResponse);
            }
            text.reserve_exact(bytes);
            text.push_str(PREFIX);
            text.push_str(request.text());
        }
        let response = AssistantResponse::new(assistant_id, &text).map_err(|_| FakeFailure::InvalidResponse)?;

        let mut remaining = response.text();
        let mut index = 0;
        while !remaining.is_empty() {
            #[cfg(feature = "test-api")]
            if self.test.as_ref().and_then(|test| test.fail_after) == Some(index) {
                return Err(FakeFailure::Adapter);
            }
            let end = remaining.floor_char_boundary(MAX_CHUNK_BYTES.min(remaining.len()));
            let (part, rest) = remaining.split_at(end);
            progress.try_send(Chunk { index, text: part.to_owned() }).map_err(|error| match error {
                TrySendError::Full(_) => FakeFailure::ProgressFull,
                TrySendError::Disconnected(_) => FakeFailure::ProgressDisconnected,
            })?;
            remaining = rest;
            index += 1;
        }
        #[cfg(feature = "test-api")]
        if self.test.as_ref().and_then(|test| test.fail_after) == Some(index) {
            return Err(FakeFailure::Adapter);
        }
        Ok(FakeOutcome::Response(response))
    }
}

#[cfg(feature = "test-api")]
use std::sync::{Arc, Condvar, Mutex, atomic::AtomicUsize};

#[cfg(feature = "test-api")]
#[derive(Clone, Default)]
pub struct TestFake {
    reply: Option<Arc<str>>,
    pause: Option<FakePause>,
    fail_after: Option<usize>,
    invocations: Arc<AtomicUsize>,
}

#[cfg(feature = "test-api")]
impl TestFake {
    pub fn with_reply(reply: &str) -> Result<Self, yokai_types::TextLength> {
        if !(1..=MAX_RESPONSE_BYTES).contains(&reply.len()) {
            return Err(yokai_types::TextLength);
        }
        Ok(Self { reply: Some(Arc::from(reply)), ..Self::default() })
    }

    pub fn failing_after(chunks: usize) -> Self {
        Self { fail_after: Some(chunks), ..Self::default() }
    }

    pub fn paused() -> (Self, FakePause) {
        let pause = FakePause(Arc::new((Mutex::new(None), Condvar::new())));
        (Self { pause: Some(pause.clone()), ..Self::default() }, pause)
    }

    pub fn invocations(&self) -> usize {
        self.invocations.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(feature = "test-api")]
#[derive(Clone)]
pub struct FakePause(Arc<(Mutex<Option<bool>>, Condvar)>);

#[cfg(feature = "test-api")]
impl FakePause {
    pub fn resume(self) { self.release(true); }
    pub fn interrupt(self) { self.release(false); }

    fn release(self, run: bool) {
        let (state, changed) = &*self.0;
        *state.lock().unwrap() = Some(run);
        changed.notify_all();
    }

    fn run_before(&self, deadline: Instant) -> bool {
        let (state, changed) = &*self.0;
        let state = state.lock().unwrap();
        let (state, _) = changed.wait_timeout_while(state, deadline.saturating_duration_since(Instant::now()), |state| state.is_none()).unwrap();
        *state == Some(true)
    }
}

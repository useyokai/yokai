//! One synthetic conversation through the in-process service.
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};
use yokai_exec::{ConversationRuntime, Submission};
use yokai_store::{create_instance, open_instance};
use yokai_types::{
    CommandId, ConversationDecision as Decision, ConversationEventKind as Event, ConversationId,
    CreateConversation, MessageId, ReplayCursor, RunState, SubmitMessage,
};

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [parent_flag, parent, message_flag, message] = args.as_slice() else {
        return Err("usage: conversation_core --parent <existing directory> --message <text>".into());
    };
    if parent_flag != "--parent" || message_flag != "--message" {
        return Err("expected --parent followed by --message".into());
    }
    let parent = PathBuf::from(parent);
    if !parent.is_absolute() || !parent.is_dir() {
        return Err("parent must be an existing absolute directory for synthetic data".into());
    }
    let input = message.to_str().ok_or("message must be valid UTF-8")?;
    let id = ConversationId::from_bytes([1; 16]);
    let user_id = MessageId::from_bytes([2; 16]);
    let assistant_id = MessageId::from_bytes([3; 16]);
    let request = SubmitMessage::new(CommandId::new("demo-message")?, id, user_id, input)?;
    let fixture = parent.join("conversation-core-demo");
    std::fs::create_dir(&fixture)?;

    let instance = create_instance(&fixture)?;
    let root = instance.root().to_owned();
    let cursor = ReplayCursor::start(instance.id(), id);
    let mut runtime = ConversationRuntime::new(instance);
    let client = runtime.handle();
    let creation = client.create_conversation(CreateConversation {
        command_id: CommandId::new("demo-create")?,
        conversation_id: id,
    }, deadline())?;
    if !matches!(creation, Decision::Created { .. }) {
        return Err(format!("create refused: {creation:?}").into());
    }

    let end = deadline();
    let Submission::Streaming { ticket, stream } = client.submit_message(request.clone(), assistant_id, end)? else {
        return Err("message did not start streaming".into());
    };
    let mut output = String::new();
    let mut index = 0;
    loop {
        match stream.progress.recv_timeout(end.saturating_duration_since(Instant::now())) {
            Ok(chunk) => {
                if chunk.index != index {
                    return Err("unexpected chunk sequence".into());
                }
                println!("chunk {} {:?}", chunk.index, chunk.text);
                output.push_str(&chunk.text);
                index += 1;
            }
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => return Err(io::Error::new(io::ErrorKind::TimedOut, "progress deadline").into()),
        }
    }
    if stream.completion.wait(end)? != (Decision::Completed { run_id: ticket.run_id }) || output != format!("fake: {input}") {
        return Err("expected a confirmed fake response".into());
    }
    let prior = client.replay_events(id, cursor, 20, deadline())?;
    runtime.close(deadline())?;

    let mut reopened = ConversationRuntime::new(open_instance(&root)?);
    let client = reopened.handle();
    let replay = client.replay_events(id, cursor, 20, deadline())?;
    if replay != prior || replay.has_more || replay.events.len() != 3 {
        return Err("reopened conversation differs".into());
    }
    if !matches!(&replay.events[0].kind, Event::Created)
        || !matches!(&replay.events[1].kind, Event::Submitted(message) if message.id == user_id && message.text == input)
        || !matches!(&replay.events[2].kind, Event::Completed(message) if message.id == assistant_id && message.text == output)
    {
        return Err("replay did not reproduce the committed messages".into());
    }
    if !matches!(client.submit_message(request, assistant_id, deadline())?,
        Submission::Decision(Decision::Duplicate { run_id, state: RunState::Completed }) if run_id == ticket.run_id)
    {
        return Err("repeat did not resolve to the original completed run".into());
    }
    reopened.close(deadline())?;
    println!("CHECK M1-DEMO PASS confirmed completion, equal replay after reopen, duplicate resolved");
    Ok(())
}

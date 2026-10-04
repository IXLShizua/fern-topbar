use super::{Command, Workspace};
use crate::backend::{Event as BackendEvent, reconnect::ReconnectBackoff};
use crate::features::{
    FeatureId,
    availability::{self, Availability, AvailabilityPublisher, ProbeError, UnavailableReason},
};
use crate::runtime;
use niri_ipc::state::{EventStreamStatePart, WorkspacesState};
use niri_ipc::{Action, Event, Reply, Request, Response, WorkspaceReferenceArg};
use snafu::Snafu;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Snafu)]
pub enum Error {
    #[snafu(display("Cannot connect to Niri"))]
    Connect,

    #[snafu(display("Niri request timed out"))]
    Timeout,

    #[snafu(display("Cannot access Niri socket"))]
    PermissionDenied,

    #[snafu(display("Cannot write a Niri request"))]
    Write,

    #[snafu(display("Cannot read from Niri"))]
    Read,

    #[snafu(display("Cannot encode a Niri request"))]
    EncodeRequest,

    #[snafu(display("Cannot decode a Niri reply"))]
    DecodeReply,

    #[snafu(display("Cannot decode a Niri event"))]
    DecodeEvent,

    #[snafu(display("Niri closed the connection"))]
    Disconnected,

    #[snafu(display("Niri rejected the request"))]
    Rejected,

    #[snafu(display("Unexpected Niri response"))]
    UnexpectedResponse,

    #[snafu(display("Niri event receiver closed"))]
    ReceiverClosed,
}

type Result<T> = std::result::Result<T, Error>;

pub fn start(
    socket_path: PathBuf,
    events: UnboundedSender<BackendEvent>,
    availability: availability::FeatureAvailability,
) -> UnboundedSender<Command> {
    let (commands, command_receiver) = mpsc::unbounded_channel();
    let writer = CommandWriter::new(socket_path.clone(), command_receiver);
    let reader = EventReader::new(socket_path, events).with_availability(availability);

    runtime::spawn(reader.run());
    runtime::spawn(writer.run());

    commands
}

pub struct CommandWriter {
    socket_path: PathBuf,
    commands: UnboundedReceiver<Command>,
    stream: Option<BufReader<UnixStream>>,
}

impl CommandWriter {
    pub fn new(socket_path: PathBuf, commands: UnboundedReceiver<Command>) -> Self {
        Self {
            socket_path,
            commands,
            stream: None,
        }
    }

    pub async fn run(mut self) {
        while let Some(command) = self.commands.recv().await {
            let _ = self.execute(command).await;
        }
    }

    pub async fn execute(&mut self, command: Command) -> Result<()> {
        let request = Request::Action(match command {
            Command::FocusWorkspace(id) => Action::FocusWorkspace {
                reference: WorkspaceReferenceArg::Id(id),
            },
            Command::CloseOverview => Action::CloseOverview {},
        });

        let reusing_connection = self.stream.is_some();
        let reply = match self.send(&request).await {
            Err(Error::Write | Error::Read | Error::Disconnected | Error::Timeout)
                if reusing_connection =>
            {
                self.send(&request).await?
            }
            result => result?,
        };

        handled(reply)
    }

    async fn send(&mut self, request: &Request) -> Result<Reply> {
        let mut stream = match self.stream.take() {
            Some(stream) => stream,
            None => connect(&self.socket_path).await?,
        };

        write_request(&mut stream, request).await?;

        let reply = read_reply(&mut stream).await?;

        self.stream = Some(stream);

        Ok(reply)
    }
}

pub struct EventReader {
    socket_path: PathBuf,
    events: UnboundedSender<BackendEvent>,
    availability: [AvailabilityPublisher; 2],
}

impl EventReader {
    pub fn new(socket_path: PathBuf, events: UnboundedSender<BackendEvent>) -> Self {
        Self {
            socket_path,
            events,
            availability: std::array::from_fn(|_| AvailabilityPublisher::default()),
        }
    }

    pub fn with_availability(mut self, availability: availability::FeatureAvailability) -> Self {
        self.availability = [
            availability.publisher(FeatureId::Workspaces),
            availability.publisher(FeatureId::KeyboardLayout),
        ];

        self
    }

    pub async fn run(self) {
        let mut retry = ReconnectBackoff::default();

        loop {
            let result = tokio::select! {
                biased;
                _ = self.events.closed() => return,
                result = self.read_events() => result,
            };

            if self
                .availability
                .iter()
                .any(|publisher| publisher.current().is_available())
            {
                retry.reset();
            }

            let state = match result {
                Err(Error::ReceiverClosed) => return,
                Err(Error::Connect) => Availability::Unavailable(UnavailableReason::ServiceMissing),
                Err(Error::PermissionDenied) => Availability::Failed(ProbeError::PermissionDenied),
                Err(Error::Timeout) => Availability::Failed(ProbeError::Timeout),
                Err(
                    Error::DecodeEvent
                    | Error::DecodeReply
                    | Error::UnexpectedResponse
                    | Error::Rejected,
                ) => Availability::Failed(ProbeError::Protocol),
                _ => Availability::Failed(ProbeError::Connect),
            };

            for publisher in &self.availability {
                publisher.set(state);
            }

            let _ = self
                .events
                .send(BackendEvent::WorkspacesChanged(Vec::new()));
            let _ = self.events.send(BackendEvent::KeyboardLayoutChanged(None));

            tokio::select! {
                biased;
                _ = self.events.closed() => return,
                _ = tokio::time::sleep(retry.next_delay()) => {}
            }
        }
    }

    async fn read_events(&self) -> Result<()> {
        let reader = self.connect_event_stream().await?;
        let mut lines = reader.lines();
        let mut state = StreamState::default();

        let deadline = tokio::time::Instant::now() + availability::PROBE_TIMEOUT;
        let mut initialized = [false; 2];

        loop {
            let line = if initialized.iter().all(|ready| *ready) {
                lines.next_line().await
            } else {
                tokio::time::timeout_at(deadline, lines.next_line())
                    .await
                    .map_err(|_| Error::Timeout)?
            };

            let Some(line) = line.map_err(|error| {
                tracing::warn!(%error, "cannot read niri event");

                Error::Read
            })?
            else {
                break;
            };

            let event: Event = serde_json::from_str(&line).map_err(|error| {
                tracing::warn!(%error, "cannot decode niri event");

                Error::DecodeEvent
            })?;

            if let Some(event) = state.handle(event) {
                match &event {
                    BackendEvent::WorkspacesChanged(_) => {
                        initialized[0] = true;
                        self.availability[0].set(Availability::Available);
                    }
                    BackendEvent::KeyboardLayoutChanged(layout) => {
                        initialized[1] = true;
                        self.availability[1].set(if layout.is_some() {
                            Availability::Available
                        } else {
                            Availability::Unavailable(UnavailableReason::DeviceMissing)
                        });
                    }
                    _ => {}
                }

                self.events.send(event).map_err(|error| {
                    tracing::debug!(%error, "niri event receiver closed");

                    Error::ReceiverClosed
                })?;
            }
        }

        tracing::debug!("niri closed the event stream");

        DisconnectedSnafu.fail()
    }

    async fn connect_event_stream(&self) -> Result<BufReader<UnixStream>> {
        let mut stream = connect(&self.socket_path).await?;

        write_request(&mut stream, &Request::EventStream).await?;
        handled(read_reply(&mut stream).await?)?;

        Ok(stream)
    }
}

async fn connect(path: &PathBuf) -> Result<BufReader<UnixStream>> {
    let stream = tokio::time::timeout(availability::PROBE_TIMEOUT, UnixStream::connect(path))
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(|error| {
            tracing::debug!(%error, path = %path.display(), "cannot connect to niri");

            if error.kind() == std::io::ErrorKind::PermissionDenied {
                Error::PermissionDenied
            } else {
                Error::Connect
            }
        })?;

    Ok(BufReader::new(stream))
}

async fn write_request(stream: &mut BufReader<UnixStream>, request: &Request) -> Result<()> {
    let mut message = serde_json::to_vec(request).map_err(|error| {
        tracing::warn!(%error, ?request, "cannot encode niri request");

        Error::EncodeRequest
    })?;

    message.push(b'\n');

    tokio::time::timeout(
        availability::PROBE_TIMEOUT,
        stream.get_mut().write_all(&message),
    )
    .await
    .map_err(|_| Error::Timeout)?
    .map_err(|error| {
        tracing::warn!(%error, ?request, "cannot write niri request");

        Error::Write
    })
}

async fn read_reply(stream: &mut BufReader<UnixStream>) -> Result<Reply> {
    let mut message = String::new();
    let size = tokio::time::timeout(availability::PROBE_TIMEOUT, stream.read_line(&mut message))
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(|error| {
            tracing::warn!(%error, "cannot read niri reply");

            Error::Read
        })?;

    if size == 0 {
        tracing::debug!("niri closed the reply connection");
        return DisconnectedSnafu.fail();
    }

    serde_json::from_str::<Reply>(&message).map_err(|error| {
        tracing::warn!(%error, "cannot decode niri reply");

        Error::DecodeReply
    })
}

fn handled(reply: Reply) -> Result<()> {
    match reply {
        Ok(Response::Handled) => Ok(()),
        Ok(response) => {
            tracing::warn!(?response, "unexpected niri response");

            UnexpectedResponseSnafu.fail()
        }
        Err(message) => {
            tracing::warn!(%message, "niri rejected the request");

            RejectedSnafu.fail()
        }
    }
}

#[derive(Default)]
struct StreamState {
    workspaces: WorkspacesState,
    keyboard_layouts: Vec<String>,
}

impl StreamState {
    fn handle(&mut self, event: Event) -> Option<BackendEvent> {
        if self.references_unknown_workspace(&event) {
            return None;
        }

        let workspaces_changed = changes_workspaces(&event);

        self.workspaces.apply(event.clone());

        if workspaces_changed {
            return Some(BackendEvent::WorkspacesChanged(self.snapshot_workspaces()));
        }

        self.handle_desktop_state_event(event)
    }

    fn references_unknown_workspace(&self, event: &Event) -> bool {
        matches!(
            event,
            Event::WorkspaceActivated { id, .. }
                if !self.workspaces.workspaces.contains_key(id)
        )
    }

    fn snapshot_workspaces(&self) -> Vec<Workspace> {
        let mut workspaces: Vec<_> = self
            .workspaces
            .workspaces
            .values()
            .map(|workspace| Workspace {
                id: workspace.id,
                index: workspace.idx,
                name: workspace.name.clone(),
                output: workspace.output.clone(),
                active: workspace.is_active,
                urgent: workspace.is_urgent,
            })
            .collect();

        workspaces.sort_by(|a, b| (&a.output, a.index, a.id).cmp(&(&b.output, b.index, b.id)));

        workspaces
    }

    fn handle_desktop_state_event(&mut self, event: Event) -> Option<BackendEvent> {
        match event {
            Event::OverviewOpenedOrClosed { is_open: true } => {
                Some(BackendEvent::OverviewOpened(self.focused_output()))
            }
            Event::OverviewOpenedOrClosed { is_open: false } => Some(BackendEvent::OverviewClosed),
            Event::KeyboardLayoutsChanged { keyboard_layouts } => {
                self.keyboard_layouts = keyboard_layouts.names;

                Some(self.keyboard_layout_changed(keyboard_layouts.current_idx))
            }
            Event::KeyboardLayoutSwitched { idx } => Some(self.keyboard_layout_changed(idx)),
            _ => None,
        }
    }

    fn focused_output(&self) -> Option<String> {
        self.workspaces
            .workspaces
            .values()
            .find(|workspace| workspace.is_focused)
            .and_then(|workspace| workspace.output.clone())
    }

    fn keyboard_layout_changed(&self, index: u8) -> BackendEvent {
        BackendEvent::KeyboardLayoutChanged(self.keyboard_layouts.get(usize::from(index)).cloned())
    }
}

fn changes_workspaces(event: &Event) -> bool {
    matches!(
        event,
        Event::WorkspacesChanged { .. }
            | Event::WorkspaceActivated { .. }
            | Event::WorkspaceUrgencyChanged { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::Duration};
    use tokio::{net::UnixListener, sync::mpsc, time::timeout};

    fn command_writer(path: PathBuf) -> CommandWriter {
        let (_, commands) = mpsc::unbounded_channel();

        CommandWriter::new(path, commands)
    }

    fn reply_message(reply: Reply) -> Vec<u8> {
        let mut message = serde_json::to_vec(&reply).unwrap();
        message.push(b'\n');

        message
    }

    #[tokio::test]
    async fn publishes_readiness_after_a_late_socket_and_restores_it_after_disconnect() {
        use crate::features::availability::tests::wait_for;

        let directory =
            std::env::temp_dir().join(format!("fern-topbar-niri-readiness-{}", std::process::id()));

        fs::create_dir_all(&directory).unwrap();

        let path = directory.join("socket");
        let availability = availability::FeatureAvailability::default();
        let mut workspaces = availability.subscribe(FeatureId::Workspaces);
        let mut layout = availability.subscribe(FeatureId::KeyboardLayout);
        let (events, receiver) = mpsc::unbounded_channel();
        let reader = EventReader::new(path.clone(), events).with_availability(availability);
        let worker = tokio::spawn(reader.run());

        wait_for(
            &mut workspaces,
            Availability::Unavailable(UnavailableReason::ServiceMissing),
        )
        .await;

        let listener = UnixListener::bind(&path).unwrap();

        for _ in 0..2 {
            let (stream, _) = timeout(Duration::from_secs(2), listener.accept())
                .await
                .unwrap()
                .unwrap();

            let mut stream = BufReader::new(stream);
            let mut request = String::new();

            stream.read_line(&mut request).await.unwrap();
            assert!(matches!(
                serde_json::from_str::<Request>(&request).unwrap(),
                Request::EventStream
            ));
            stream
                .get_mut()
                .write_all(&reply_message(Ok(Response::Handled)))
                .await
                .unwrap();

            for event in [
                Event::WorkspacesChanged {
                    workspaces: Vec::new(),
                },
                Event::KeyboardLayoutsChanged {
                    keyboard_layouts: niri_ipc::KeyboardLayouts {
                        names: vec!["English".into()],
                        current_idx: 0,
                    },
                },
            ] {
                let mut message = serde_json::to_vec(&event).unwrap();
                message.push(b'\n');
                stream.get_mut().write_all(&message).await.unwrap();
            }

            wait_for(&mut workspaces, Availability::Available).await;
            wait_for(&mut layout, Availability::Available).await;
            drop(stream);
            wait_for(&mut workspaces, Availability::Failed(ProbeError::Connect)).await;
            wait_for(&mut layout, Availability::Failed(ProbeError::Connect)).await;
        }

        drop(receiver);
        timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();
        drop(listener);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn reuses_the_command_connection_and_keeps_it_after_action_errors() {
        let directory =
            std::env::temp_dir().join(format!("fern-topbar-niri-commands-{}", std::process::id()));

        fs::create_dir_all(&directory).unwrap();

        let path = directory.join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let (commands, receiver) = mpsc::unbounded_channel();
        let writer = CommandWriter::new(path, receiver);
        let worker = tokio::spawn(writer.run());

        commands.send(Command::CloseOverview).unwrap();
        commands.send(Command::FocusWorkspace(42)).unwrap();
        commands.send(Command::CloseOverview).unwrap();

        drop(commands);

        let (stream, _) = timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();

        let mut reader = BufReader::new(stream);

        for index in 0..3 {
            let mut request = String::new();

            timeout(Duration::from_secs(1), reader.read_line(&mut request))
                .await
                .unwrap()
                .unwrap();

            let request = serde_json::from_str::<Request>(&request).unwrap();

            match index {
                1 => assert!(matches!(
                    request,
                    Request::Action(Action::FocusWorkspace {
                        reference: WorkspaceReferenceArg::Id(42),
                    })
                )),
                _ => assert!(matches!(request, Request::Action(Action::CloseOverview {}))),
            }

            let reply: Reply = if index == 1 {
                Err("action rejected".into())
            } else {
                Ok(Response::Handled)
            };

            reader
                .get_mut()
                .write_all(&reply_message(reply))
                .await
                .unwrap();
        }

        timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();

        let mut remaining = String::new();

        assert_eq!(reader.read_line(&mut remaining).await.unwrap(), 0);

        drop(reader);
        drop(listener);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn reconnects_the_command_writer_when_the_socket_is_replaced() {
        let directory =
            std::env::temp_dir().join(format!("fern-topbar-niri-writer-{}", std::process::id()));

        fs::create_dir_all(&directory).unwrap();

        let path = directory.join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let mut writer = command_writer(path.clone());
        let worker = tokio::spawn(async move {
            for id in [1, 2] {
                writer.execute(Command::FocusWorkspace(id)).await.unwrap();
            }
        });

        let (stream, _) = timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();

        let mut reader = BufReader::new(stream);
        let mut request = String::new();

        reader.read_line(&mut request).await.unwrap();

        assert!(matches!(
            serde_json::from_str::<Request>(&request).unwrap(),
            Request::Action(Action::FocusWorkspace {
                reference: WorkspaceReferenceArg::Id(1),
            })
        ));

        drop(listener);
        fs::remove_file(&path).unwrap();

        let listener = UnixListener::bind(&path).unwrap();

        reader
            .get_mut()
            .write_all(&reply_message(Ok(Response::Handled)))
            .await
            .unwrap();

        drop(reader);

        let (stream, _) = timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();

        let mut reader = BufReader::new(stream);

        request.clear();
        reader.read_line(&mut request).await.unwrap();

        assert!(matches!(
            serde_json::from_str::<Request>(&request).unwrap(),
            Request::Action(Action::FocusWorkspace {
                reference: WorkspaceReferenceArg::Id(2),
            })
        ));

        reader
            .get_mut()
            .write_all(&reply_message(Ok(Response::Handled)))
            .await
            .unwrap();

        timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();

        drop(reader);
        drop(listener);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn connects_after_an_initial_connection_failure() {
        let directory = std::env::temp_dir().join(format!(
            "fern-topbar-niri-writer-start-{}",
            std::process::id()
        ));

        fs::create_dir_all(&directory).unwrap();

        let path = directory.join("socket");
        let mut writer = command_writer(path.clone());

        assert!(writer.execute(Command::CloseOverview).await.is_err());

        let listener = UnixListener::bind(&path).unwrap();
        let worker = tokio::spawn(async move {
            writer.execute(Command::CloseOverview).await.unwrap();
        });
        let (stream, _) = timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();

        let mut reader = BufReader::new(stream);
        let mut request = String::new();

        reader.read_line(&mut request).await.unwrap();

        assert!(matches!(
            serde_json::from_str::<Request>(&request).unwrap(),
            Request::Action(Action::CloseOverview {})
        ));

        reader
            .get_mut()
            .write_all(&reply_message(Ok(Response::Handled)))
            .await
            .unwrap();

        timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();

        drop(reader);
        drop(listener);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn reconnects_the_event_reader_after_eof_without_replacing_the_socket() {
        let directory = std::env::temp_dir().join(format!(
            "fern-topbar-niri-reader-eof-{}",
            std::process::id()
        ));

        fs::create_dir_all(&directory).unwrap();

        let path = directory.join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let (events, mut receiver) = mpsc::unbounded_channel();
        let worker = tokio::spawn(EventReader::new(path, events).run());

        for is_open in [true, false] {
            let (stream, _) = timeout(Duration::from_secs(1), listener.accept())
                .await
                .unwrap()
                .unwrap();

            let mut reader = BufReader::new(stream);
            let mut request = String::new();

            reader.read_line(&mut request).await.unwrap();

            assert!(matches!(
                serde_json::from_str::<Request>(&request).unwrap(),
                Request::EventStream
            ));

            let event = serde_json::to_string(&Event::OverviewOpenedOrClosed { is_open }).unwrap();

            reader
                .get_mut()
                .write_all(&reply_message(Ok(Response::Handled)))
                .await
                .unwrap();

            reader
                .get_mut()
                .write_all(format!("{event}\n").as_bytes())
                .await
                .unwrap();

            timeout(Duration::from_secs(1), async {
                loop {
                    match receiver.recv().await.unwrap() {
                        BackendEvent::OverviewOpened(_) if is_open => break,
                        BackendEvent::OverviewClosed if !is_open => break,
                        _ => {}
                    }
                }
            })
            .await
            .unwrap();

            drop(reader);
        }

        drop(receiver);
        timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();

        drop(listener);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn rejects_an_unsuccessful_event_subscription() {
        let directory = std::env::temp_dir().join(format!(
            "fern-topbar-niri-reader-reject-{}",
            std::process::id()
        ));

        fs::create_dir_all(&directory).unwrap();

        let path = directory.join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let (events, _receiver) = mpsc::unbounded_channel();
        let reader = EventReader::new(path, events);
        let worker = tokio::spawn(async move {
            let error = reader.connect_event_stream().await.unwrap_err();

            assert_eq!(error, Error::Rejected);
        });

        let (stream, _) = timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();

        let mut stream = BufReader::new(stream);
        let mut request = String::new();

        stream.read_line(&mut request).await.unwrap();
        stream
            .get_mut()
            .write_all(&reply_message(Err("subscription rejected".into())))
            .await
            .unwrap();

        timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();

        drop(stream);
        drop(listener);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn stops_while_waiting_for_the_event_subscription_reply() {
        let directory = std::env::temp_dir().join(format!(
            "fern-topbar-niri-reader-stop-{}",
            std::process::id()
        ));

        fs::create_dir_all(&directory).unwrap();

        let path = directory.join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let (events, receiver) = mpsc::unbounded_channel();
        let worker = tokio::spawn(EventReader::new(path, events).run());
        let (stream, _) = timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();

        let mut stream = BufReader::new(stream);
        let mut request = String::new();

        stream.read_line(&mut request).await.unwrap();

        drop(receiver);
        timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(stream.read_line(&mut request).await.unwrap(), 0);

        drop(stream);
        drop(listener);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn reconnects_when_the_socket_is_replaced_and_stops_when_unused() {
        let directory =
            std::env::temp_dir().join(format!("fern-topbar-niri-{}", std::process::id()));

        fs::create_dir_all(&directory).unwrap();

        let path = directory.join("socket");
        let (events, mut receiver) = mpsc::unbounded_channel();
        let reader = EventReader::new(path.clone(), events);
        let mut listener = UnixListener::bind(&path).unwrap();
        let worker = tokio::spawn(async move { reader.run().await });

        for is_open in [true, false] {
            let (stream, _) = timeout(Duration::from_secs(1), listener.accept())
                .await
                .unwrap()
                .unwrap();

            let mut reader = BufReader::new(stream);
            let mut request = String::new();

            reader.read_line(&mut request).await.unwrap();

            assert!(matches!(
                serde_json::from_str::<Request>(&request).unwrap(),
                Request::EventStream
            ));

            reader
                .get_mut()
                .write_all(&reply_message(Ok(Response::Handled)))
                .await
                .unwrap();

            let event = serde_json::to_string(&Event::OverviewOpenedOrClosed { is_open }).unwrap();

            reader
                .get_mut()
                .write_all(format!("{event}\n").as_bytes())
                .await
                .unwrap();

            timeout(Duration::from_secs(1), async {
                loop {
                    match receiver.recv().await.unwrap() {
                        BackendEvent::OverviewOpened(_) if is_open => break,
                        BackendEvent::OverviewClosed if !is_open => break,
                        _ => {}
                    }
                }
            })
            .await
            .unwrap();

            drop(reader);

            if is_open {
                drop(listener);
                fs::remove_file(&path).unwrap();
                listener = UnixListener::bind(&path).unwrap();
            }
        }

        drop(receiver);
        timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();

        drop(listener);
        fs::remove_file(&path).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}

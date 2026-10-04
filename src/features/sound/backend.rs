use super::model::{AudioDevice, Volume};
use crate::backend::reconnect::ReconnectBackoff;
use crate::features::availability::{
    Availability, AvailabilityPublisher, ProbeError, UnavailableReason,
};
use pulse::Message;
use std::sync::Arc;
use std::sync::mpsc::{self as std_mpsc, Sender};
use std::thread;

pub struct Backend {
    controls: Controls,
    events: Option<tokio::sync::mpsc::UnboundedReceiver<Event>>,
}

#[derive(Clone, Copy, Debug)]
pub struct Event {
    pub device: AudioDevice,
    pub volume: Option<Volume>,
}

#[derive(Clone)]
pub struct Controls {
    messages: Arc<ControlChannel>,
}

impl Controls {
    pub fn set_volume(&self, device: AudioDevice, level: u8) {
        let _ = self
            .messages
            .messages
            .send(Message::SetVolume(device, level.min(100)));
    }

    pub fn toggle_mute(&self, device: AudioDevice) {
        let _ = self.messages.messages.send(Message::ToggleMute(device));
    }
}

impl Backend {
    pub fn start(availability: [AvailabilityPublisher; 2]) -> Self {
        let (event_sender, events) = tokio::sync::mpsc::unbounded_channel();
        let (message_sender, messages) = std_mpsc::channel();
        let controls = Controls {
            messages: Arc::new(ControlChannel {
                messages: message_sender.clone(),
            }),
        };

        // PulseAudio's threaded main loop runs outside Tokio. Dropping the last
        // Controls sends Shutdown; retaining a JoinHandle would only detach it
        // on drop and adds no lifetime or cancellation guarantees.
        thread::spawn(move || {
            pulse::run(event_sender, message_sender, messages, availability);
        });

        Self {
            controls,
            events: Some(events),
        }
    }

    pub fn controls(&self) -> Controls {
        self.controls.clone()
    }

    pub fn take_events(&mut self) -> tokio::sync::mpsc::UnboundedReceiver<Event> {
        self.events.take().expect("audio events taken once")
    }
}

// The driver keeps its own sender for native callbacks, so channel closure
// cannot signal shutdown. Only the final external control owner sends it.
struct ControlChannel {
    messages: Sender<Message>,
}

impl Drop for ControlChannel {
    fn drop(&mut self) {
        let _ = self.messages.send(Message::Shutdown);
    }
}

#[cfg(test)]
pub mod tests {
    pub use super::pulse::Message;
    use super::{Arc, ControlChannel, Controls, std_mpsc};

    /// Supplies real control messages without starting a PulseAudio connection.
    pub fn controls() -> (Controls, std_mpsc::Receiver<Message>) {
        let (messages, receiver) = std_mpsc::channel();
        let controls = Controls {
            messages: Arc::new(ControlChannel { messages }),
        };

        (controls, receiver)
    }
}

mod pulse {
    use super::{
        AudioDevice, Availability, AvailabilityPublisher, Event, ProbeError, ReconnectBackoff,
        UnavailableReason, Volume,
    };
    use libpulse_binding as pulse;
    use pulse::callbacks::ListResult;
    use pulse::context::subscribe::{Facility, InterestMaskSet};
    use pulse::context::{Context, FlagSet, State};
    use pulse::error::Code;
    use pulse::mainloop::threaded::Mainloop;
    use pulse::volume::{ChannelVolumes, Volume as PulseVolume};
    use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
    use std::time::{Duration, Instant};

    pub enum Message {
        SetVolume(AudioDevice, u8),
        ToggleMute(AudioDevice),
        Shutdown,
        StateChanged(u64),
        SubscriptionFailed(u64),
        ServerChanged(u64),
        DeviceChanged(u64, AudioDevice),
        DefaultDevice(u64, AudioDevice, Option<String>),
        DeviceInfo(u64, AudioDevice, String, Option<DeviceState>),
    }

    pub struct DeviceState {
        channels: u8,
        volume: Volume,
    }

    #[derive(Default)]
    struct DeviceSlot {
        name: Option<String>,
        state: Option<DeviceState>,
        last_volume: Option<Volume>,
        initialized: bool,
        query: Option<String>,
        dirty: bool,
    }

    enum SessionEnd {
        Disconnected,
        Stopped,
    }

    pub fn run(
        events: tokio::sync::mpsc::UnboundedSender<Event>,
        message_sender: Sender<Message>,
        messages: Receiver<Message>,
        availability: [AvailabilityPublisher; 2],
    ) {
        AudioDriver::new(events, message_sender, messages, availability).run();
    }

    struct AudioDriver {
        events: tokio::sync::mpsc::UnboundedSender<Event>,
        message_sender: Sender<Message>,
        messages: Receiver<Message>,
        generation: u64,
        availability: [AvailabilityPublisher; 2],
    }

    impl AudioDriver {
        fn new(
            events: tokio::sync::mpsc::UnboundedSender<Event>,
            message_sender: Sender<Message>,
            messages: Receiver<Message>,
            availability: [AvailabilityPublisher; 2],
        ) -> Self {
            Self {
                events,
                message_sender,
                messages,
                generation: 0,
                availability,
            }
        }

        fn run(&mut self) {
            let mut retry = ReconnectBackoff::default();

            loop {
                self.generation = self.generation.wrapping_add(1);

                let session = PulseSession::connect(
                    &self.events,
                    &self.message_sender,
                    &self.messages,
                    self.generation,
                    &self.availability,
                );

                let mut session = match session {
                    Ok(session) => session,
                    Err(state) => {
                        for publisher in &self.availability {
                            publisher.set(state);
                        }

                        if !self.publish_disconnected() || !self.wait_retry(retry.next_delay()) {
                            return;
                        }

                        continue;
                    }
                };

                let outcome = session.run();

                if session.state.devices.iter().all(|slot| slot.initialized) {
                    retry.reset();
                }

                drop(session);

                if !self.publish_disconnected() || matches!(outcome, SessionEnd::Stopped) {
                    return;
                }

                if !self.wait_retry(retry.next_delay()) {
                    return;
                }
            }
        }

        fn wait_retry(&self, delay: Duration) -> bool {
            let deadline = Instant::now() + delay;

            loop {
                match self
                    .messages
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                {
                    Ok(Message::Shutdown) | Err(RecvTimeoutError::Disconnected) => return false,
                    Err(RecvTimeoutError::Timeout) => return true,
                    Ok(_) => {}
                }
            }
        }

        fn publish_disconnected(&self) -> bool {
            [AudioDevice::Output, AudioDevice::Input]
                .into_iter()
                .all(|device| {
                    if self.availability[device.index()].current().is_available() {
                        self.availability[device.index()]
                            .set(Availability::Failed(ProbeError::Connect));
                    }

                    self.events
                        .send(Event {
                            device,
                            volume: None,
                        })
                        .is_ok()
                })
        }
    }

    struct PulseSession<'a> {
        mainloop: Mainloop,
        context: Context,
        events: &'a tokio::sync::mpsc::UnboundedSender<Event>,
        message_sender: &'a Sender<Message>,
        messages: &'a Receiver<Message>,
        // Native callbacks can enqueue replies after a reconnect. The generation
        // prevents the old session's replies from changing current device state.
        generation: u64,
        state: SessionState,
        availability: &'a [AvailabilityPublisher; 2],
    }

    #[derive(Default)]
    struct SessionState {
        devices: [DeviceSlot; 2],
        subscribed: bool,
        // Volume coalescing may consume the next non-volume message. Retain it
        // here so commands and native callbacks still run in their original order.
        pending: Option<Message>,
    }

    impl<'a> PulseSession<'a> {
        fn connect(
            events: &'a tokio::sync::mpsc::UnboundedSender<Event>,
            message_sender: &'a Sender<Message>,
            messages: &'a Receiver<Message>,
            generation: u64,
            availability: &'a [AvailabilityPublisher; 2],
        ) -> Result<Self, Availability> {
            let Some(mut mainloop) = Mainloop::new() else {
                tracing::warn!("cannot create audio main loop");
                return Err(Availability::Failed(ProbeError::Connect));
            };

            let Some(mut context) = Context::new(&mainloop, "fern-topbar") else {
                tracing::warn!("cannot create audio context");
                return Err(Availability::Failed(ProbeError::Connect));
            };

            Self::register_callbacks(&mut context, message_sender, generation);

            if let Err(error) = mainloop.start() {
                tracing::warn!(%error, "cannot start audio main loop");
                return Err(Availability::Failed(ProbeError::Connect));
            }

            mainloop.lock();

            let connected = context.connect(None, FlagSet::NOAUTOSPAWN, None);

            mainloop.unlock();

            if let Err(error) = connected {
                tracing::debug!(%error, "cannot connect to audio server");
                mainloop.stop();
                return Err(connection_error(error));
            }

            Ok(Self {
                mainloop,
                context,
                events,
                message_sender,
                messages,
                generation,
                state: SessionState::default(),
                availability,
            })
        }

        fn register_callbacks(context: &mut Context, messages: &Sender<Message>, generation: u64) {
            let state_sender = messages.clone();

            context.set_state_callback(Some(Box::new(move || {
                let _ = state_sender.send(Message::StateChanged(generation));
            })));

            let change_sender = messages.clone();

            context.set_subscribe_callback(Some(Box::new(move |facility, _, _| {
                let message = match facility {
                    Some(Facility::Server) => Message::ServerChanged(generation),
                    Some(Facility::Sink) => Message::DeviceChanged(generation, AudioDevice::Output),
                    Some(Facility::Source) => {
                        Message::DeviceChanged(generation, AudioDevice::Input)
                    }
                    _ => return,
                };

                let _ = change_sender.send(message);
            })));
        }

        fn run(&mut self) -> SessionEnd {
            let deadline = Instant::now() + crate::features::availability::PROBE_TIMEOUT;

            loop {
                let initialized = self.state.devices.iter().all(|slot| slot.initialized);

                if !initialized && Instant::now() >= deadline {
                    for publisher in self.availability {
                        publisher.set(Availability::Failed(ProbeError::Timeout));
                    }

                    return SessionEnd::Disconnected;
                }

                let received = match self.state.pending.take() {
                    Some(message) => Ok(message),
                    None if initialized => self
                        .messages
                        .recv()
                        .map_err(|_| RecvTimeoutError::Disconnected),
                    None => self
                        .messages
                        .recv_timeout(deadline.saturating_duration_since(Instant::now())),
                };

                let message = match received {
                    Ok(message) => message,
                    Err(RecvTimeoutError::Disconnected) => return SessionEnd::Stopped,
                    Err(RecvTimeoutError::Timeout) => continue,
                };

                self.mainloop.lock();

                let outcome = self.process(message);

                self.mainloop.unlock();

                if let Some(outcome) = outcome {
                    return outcome;
                }
            }
        }

        fn process(&mut self, message: Message) -> Option<SessionEnd> {
            match message {
                Message::StateChanged(id) if id == self.generation => self.state_changed(),
                Message::SubscriptionFailed(id) if id == self.generation => {
                    for publisher in self.availability {
                        publisher.set(Availability::Failed(ProbeError::Read));
                    }

                    Some(SessionEnd::Disconnected)
                }
                Message::ServerChanged(id) if id == self.generation => {
                    self.request_server();

                    None
                }
                Message::DeviceChanged(id, device) if id == self.generation => {
                    self.refresh_device(device);

                    None
                }
                Message::DefaultDevice(id, device, name) if id == self.generation => {
                    self.default_device_changed(device, name)
                }
                Message::DeviceInfo(id, device, name, state) if id == self.generation => {
                    self.device_info_changed(device, name, state)
                }
                Message::SetVolume(device, level) => {
                    let level = self.coalesce_volume(device, level);

                    self.set_volume(device, level);

                    None
                }
                Message::ToggleMute(device) => {
                    self.toggle_mute(device);

                    None
                }
                Message::Shutdown => Some(SessionEnd::Stopped),
                _ => None,
            }
        }

        fn state_changed(&mut self) -> Option<SessionEnd> {
            match self.context.get_state() {
                State::Ready if !self.state.subscribed => {
                    self.state.subscribed = true;

                    let messages = self.message_sender.clone();
                    let generation = self.generation;

                    self.context.subscribe(
                        InterestMaskSet::SERVER | InterestMaskSet::SINK | InterestMaskSet::SOURCE,
                        move |success| {
                            if !success {
                                tracing::debug!("cannot subscribe to audio changes");

                                let _ = messages.send(Message::SubscriptionFailed(generation));
                            }
                        },
                    );

                    self.request_server();

                    None
                }
                State::Failed => {
                    let error = self.context.errno();

                    tracing::debug!(%error, "audio connection failed");

                    let state = connection_error(error);

                    for publisher in self.availability {
                        publisher.set(state);
                    }

                    Some(SessionEnd::Disconnected)
                }
                State::Terminated => Some(SessionEnd::Disconnected),
                _ => None,
            }
        }

        fn default_device_changed(
            &mut self,
            device: AudioDevice,
            name: Option<String>,
        ) -> Option<SessionEnd> {
            let slot = &mut self.state.devices[device.index()];

            if slot.name != name {
                slot.state = None;
            }

            slot.name = name;

            if slot.name.is_some() {
                self.refresh_device(device);
            } else if !Self::publish(device, None, slot, self.events, self.availability) {
                return Some(SessionEnd::Stopped);
            } else {
                slot.dirty = false;
            }

            None
        }

        fn device_info_changed(
            &mut self,
            device: AudioDevice,
            name: String,
            state: Option<DeviceState>,
        ) -> Option<SessionEnd> {
            let slot = &mut self.state.devices[device.index()];

            if slot.query.as_deref() == Some(name.as_str()) {
                slot.query = None;
            }

            if slot.name.as_deref() == Some(name.as_str()) {
                slot.state = state;

                let volume = slot.state.as_ref().map(|state| state.volume);

                if !Self::publish(device, volume, slot, self.events, self.availability) {
                    return Some(SessionEnd::Stopped);
                }
            }

            if slot.dirty {
                slot.dirty = false;
                self.refresh_device(device);
            }

            None
        }

        fn coalesce_volume(&mut self, device: AudioDevice, mut level: u8) -> u8 {
            while let Ok(next) = self.messages.try_recv() {
                match next {
                    Message::SetVolume(next_device, next_level) if next_device == device => {
                        level = next_level;
                    }
                    other => {
                        self.state.pending = Some(other);
                        break;
                    }
                }
            }

            level
        }

        fn set_volume(&self, device: AudioDevice, level: u8) {
            let slot = &self.state.devices[device.index()];
            let (Some(name), Some(state)) = (slot.name.as_deref(), slot.state.as_ref()) else {
                return;
            };
            let raw = (u32::from(level) * PulseVolume::NORMAL.0 + 50) / 100;
            let mut volumes = ChannelVolumes::default();
            volumes.set(state.channels, PulseVolume(raw));

            let mut introspector = self.context.introspect();

            match device {
                AudioDevice::Output => {
                    introspector.set_sink_volume_by_name(name, &volumes, None);
                }
                AudioDevice::Input => {
                    introspector.set_source_volume_by_name(name, &volumes, None);
                }
            }
        }

        fn toggle_mute(&self, device: AudioDevice) {
            let slot = &self.state.devices[device.index()];
            let (Some(name), Some(state)) = (slot.name.as_deref(), slot.state.as_ref()) else {
                return;
            };
            let mut introspector = self.context.introspect();

            match device {
                AudioDevice::Output => {
                    introspector.set_sink_mute_by_name(name, !state.volume.muted, None);
                }
                AudioDevice::Input => {
                    introspector.set_source_mute_by_name(name, !state.volume.muted, None);
                }
            }
        }

        fn request_server(&self) {
            let sender = self.message_sender.clone();
            let generation = self.generation;

            self.context.introspect().get_server_info(move |info| {
                let output = info.default_sink_name.as_ref().map(ToString::to_string);
                let input = info.default_source_name.as_ref().map(ToString::to_string);
                let _ = sender.send(Message::DefaultDevice(
                    generation,
                    AudioDevice::Output,
                    output,
                ));

                let _ = sender.send(Message::DefaultDevice(
                    generation,
                    AudioDevice::Input,
                    input,
                ));
            });
        }

        fn refresh_device(&mut self, device: AudioDevice) {
            let slot = &mut self.state.devices[device.index()];
            let Some(name) = slot.name.clone() else {
                return;
            };

            if slot.query.is_some() {
                slot.dirty = true;
                return;
            }

            slot.query = Some(name.clone());
            self.request_device(device, name);
        }

        fn request_device(&self, device: AudioDevice, name: String) {
            match device {
                AudioDevice::Output => self.request_output(name),
                AudioDevice::Input => self.request_input(name),
            }
        }

        fn request_output(&self, name: String) {
            let sender = self.message_sender.clone();
            let generation = self.generation;
            let requested_name = name.clone();
            let mut found = false;

            self.context
                .introspect()
                .get_sink_info_by_name(&requested_name, move |result| {
                    let state = match result {
                        ListResult::Item(info) => {
                            found = true;

                            Some(device_state(&info.volume, info.mute))
                        }
                        ListResult::Error => None,
                        ListResult::End if !found => None,
                        ListResult::End => return,
                    };

                    let _ = sender.send(Message::DeviceInfo(
                        generation,
                        AudioDevice::Output,
                        name.clone(),
                        state,
                    ));
                });
        }

        fn request_input(&self, name: String) {
            let sender = self.message_sender.clone();
            let generation = self.generation;
            let requested_name = name.clone();
            let mut found = false;

            self.context
                .introspect()
                .get_source_info_by_name(&requested_name, move |result| {
                    let state = match result {
                        ListResult::Item(info) => {
                            found = true;

                            Some(device_state(&info.volume, info.mute))
                        }
                        ListResult::Error => None,
                        ListResult::End if !found => None,
                        ListResult::End => return,
                    };

                    let _ = sender.send(Message::DeviceInfo(
                        generation,
                        AudioDevice::Input,
                        name.clone(),
                        state,
                    ));
                });
        }

        fn publish(
            device: AudioDevice,
            volume: Option<Volume>,
            slot: &mut DeviceSlot,
            events: &tokio::sync::mpsc::UnboundedSender<Event>,
            availability: &[AvailabilityPublisher; 2],
        ) -> bool {
            slot.initialized = true;
            availability[device.index()].set(if volume.is_some() {
                Availability::Available
            } else {
                Availability::Unavailable(UnavailableReason::DeviceMissing)
            });

            if slot.last_volume == volume {
                return true;
            }

            slot.last_volume = volume;

            events.send(Event { device, volume }).is_ok()
        }
    }

    impl Drop for PulseSession<'_> {
        fn drop(&mut self) {
            self.mainloop.lock();
            self.context.disconnect();
            self.mainloop.unlock();
            self.mainloop.stop();
        }
    }

    fn connection_error(error: pulse::error::PAErr) -> Availability {
        match Code::try_from(error) {
            Ok(Code::ConnectionRefused | Code::NoEntity) => {
                Availability::Unavailable(UnavailableReason::ServiceMissing)
            }
            Ok(Code::Access | Code::AuthKey) => Availability::Failed(ProbeError::PermissionDenied),
            Ok(Code::Timeout) => Availability::Failed(ProbeError::Timeout),
            _ => Availability::Failed(ProbeError::Connect),
        }
    }

    fn device_state(volumes: &ChannelVolumes, muted: bool) -> DeviceState {
        let level = f64::from(volumes.avg().0) / f64::from(PulseVolume::NORMAL.0);

        DeviceState {
            channels: volumes.len(),
            volume: Volume {
                level: level.clamp(0.0, 1.0),
                muted,
            },
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn absent_default_device_initializes_readiness_and_can_appear_later() {
            let publishers = std::array::from_fn(|_| AvailabilityPublisher::default());
            let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
            let mut output = DeviceSlot::default();

            assert!(PulseSession::publish(
                AudioDevice::Output,
                None,
                &mut output,
                &events,
                &publishers
            ));
            assert!(output.initialized);
            assert_eq!(
                publishers[0].current(),
                Availability::Unavailable(UnavailableReason::DeviceMissing)
            );
            assert_eq!(publishers[1].current(), Availability::Checking);

            let volume = Volume {
                level: 0.7,
                muted: false,
            };

            assert!(PulseSession::publish(
                AudioDevice::Output,
                Some(volume),
                &mut output,
                &events,
                &publishers
            ));
            assert_eq!(publishers[0].current(), Availability::Available);
            assert_eq!(receiver.try_recv().unwrap().volume, Some(volume));
            assert!(PulseSession::publish(
                AudioDevice::Output,
                None,
                &mut output,
                &events,
                &publishers
            ));
            assert_eq!(receiver.try_recv().unwrap().volume, None);
            assert_eq!(
                publishers[0].current(),
                Availability::Unavailable(UnavailableReason::DeviceMissing)
            );
        }
    }
}

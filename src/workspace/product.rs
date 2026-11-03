//! Product lifecycle for one shared hub, layout replica and native input owner.
use std::collections::BTreeSet;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::cross_screen::PeerPlacement;
use crate::cross_screen::hub::PeerConnectionHub;
use crate::cross_screen::native_input::NativeInputRuntime;
use crate::device_store::DeviceStore;
use crate::security::CertificateFingerprint;

use super::control::CrossScreenCoordinator;
use super::editor::LayoutPreview;
use super::input_runtime::{InputBackend, WorkspaceInputRuntime};
use super::persistence::{
    LocalBoundarySettings, WorkspaceConfiguration, reserve_control_epoch_range,
};
use super::sync::LayoutReplicator;
use super::{DeviceId, Edge, WorkspaceLayout};

const POLL: Duration = Duration::from_millis(5);
const FULLSCREEN_POLL: Duration = Duration::from_millis(250);

struct Session<B: InputBackend> {
    local: DeviceId,
    store: DeviceStore,
    hub: Arc<PeerConnectionHub>,
    input: WorkspaceInputRuntime<B>,
    replica: LayoutReplicator,
    applied: WorkspaceLayout,
    behavior: LocalBoundarySettings,
    paused: bool,
    fullscreen: bool,
    next_fullscreen: Instant,
    preview: Option<(u64, LayoutPreview)>,
}

impl<B: InputBackend> Session<B> {
    fn new(
        store: DeviceStore,
        hub: Arc<PeerConnectionHub>,
        config: WorkspaceConfiguration,
        edges: BTreeSet<Edge>,
        backend: B,
    ) -> io::Result<Self> {
        config.connections.validate(&store)?;
        let local = store.load_identity()?.fingerprint().bytes();
        let members = config.connections.members(local);
        let epoch_range = reserve_control_epoch_range(&store)?;
        let coordinator = CrossScreenCoordinator::new_with_epoch_range(
            local,
            config.layout.clone(),
            &members,
            edges.clone(),
            epoch_range.floor,
            epoch_range.limit,
        )?;
        let replica = LayoutReplicator::new(local, config.layout.clone(), members, edges)?;
        let input = WorkspaceInputRuntime::new(
            local,
            coordinator,
            config.layout.clone(),
            &config.behavior,
            backend,
        )?;
        Ok(Self {
            local,
            store,
            hub,
            input,
            replica,
            applied: config.layout,
            paused: !config.behavior.enabled,
            behavior: config.behavior,
            fullscreen: false,
            next_fullscreen: Instant::now(),
            preview: None,
        })
    }

    fn tick(&mut self, requested_pause: bool) -> io::Result<()> {
        let now = Instant::now();
        if now >= self.next_fullscreen {
            self.fullscreen = self.behavior.pause_on_fullscreen && self.input.fullscreen()?;
            self.next_fullscreen = now + FULLSCREEN_POLL;
        }
        let paused = requested_pause || self.fullscreen || !self.behavior.enabled;
        if self.paused != paused {
            self.input.set_paused(&self.hub, paused)?;
            self.paused = paused;
        }
        self.replica.tick(&self.store, &self.hub, &mut || {
            self.input.return_local(&self.hub)
        })?;
        if self.applied != *self.replica.layout() {
            self.input.replace_layout(
                &self.hub,
                self.replica.layout().clone(),
                self.replica.members(),
                self.replica.trusted_edges().clone(),
                &self.behavior,
            )?;
            // A geometry replacement must preserve the user's independent pause.
            self.input.set_paused(&self.hub, paused)?;
            self.applied = self.replica.layout().clone();
            self.preview = None;
        }
        self.input
            .tick(&self.hub, now, &|peer| self.replica.edge_ready(peer))
    }

    fn command(&mut self, command: Command) -> io::Result<()> {
        match command {
            Command::Preview {
                id,
                peer,
                direction,
            } => {
                self.preview = None;
                let preview = self.replica.preview_placement(peer, direction)?;
                self.preview = Some((id, preview));
                Ok(())
            }
            Command::Confirm { id } => {
                let (current, preview) = self
                    .preview
                    .as_ref()
                    .ok_or_else(|| invalid("no layout preview is pending"))?;
                if *current != id {
                    return Err(invalid("layout preview has been superseded"));
                }
                self.replica.confirm_preview(&self.hub, preview, &mut || {
                    self.input.return_local(&self.hub)
                })?;
                self.preview = None;
                Ok(())
            }
        }
    }

    fn model(&self) -> serde_json::Value {
        let token = self.input.token();
        let connections = self.hub.settings_model();
        let state = if self.fullscreen {
            "paused_fullscreen"
        } else if self.paused {
            "paused"
        } else if let Some(token) = token {
            if token.origin == self.local {
                "controlling_peer"
            } else if token.current == self.local {
                "controlled_by_peer"
            } else {
                "relaying"
            }
        } else if connections["onlineCount"].as_u64().unwrap_or(0) == 0 {
            "connecting"
        } else {
            "connected"
        };
        let fingerprint = |id| CertificateFingerprint::from_bytes(id).to_string();
        serde_json::json!({
            "workspace": self.replica.settings_model(), "connections": connections,
            "state": state, "enabled": self.behavior.enabled,
            "control": token.map(|token| serde_json::json!({
                "origin": fingerprint(token.origin), "current": fingerprint(token.current),
                "epoch": token.epoch, "session": token.session_id,
                "route": token.route.iter().copied().map(fingerprint).collect::<Vec<_>>(),
            })),
            "layoutPreview": self.preview.as_ref().map(|(id, preview)| {
                let mut model = preview.settings_model(); model["id"] = (*id).into(); model
            }),
            "lastError": self.input.last_error().or(self.replica.last_error()),
        })
    }
}

#[derive(Clone, Copy)]
enum Command {
    Preview {
        id: u64,
        peer: DeviceId,
        direction: PeerPlacement,
    },
    Confirm {
        id: u64,
    },
}

#[derive(Clone, Debug)]
pub struct WorkspaceProductSnapshot {
    pub running: bool,
    pub clean_shutdown: bool,
    pub last_error: Option<String>,
    pub model: serde_json::Value,
}

struct Shared {
    stop: AtomicBool,
    paused: AtomicBool,
    return_local: AtomicBool,
    preview_id: AtomicU64,
    snapshot: Mutex<WorkspaceProductSnapshot>,
    wake: Condvar,
}

pub struct WorkspaceProductRuntime {
    shared: Arc<Shared>,
    commands: mpsc::SyncSender<Command>,
    hub: Arc<PeerConnectionHub>,
    worker: Option<JoinHandle<()>>,
}

impl WorkspaceProductRuntime {
    /// Start one listener, one input owner and one serialized product pump.
    ///
    /// # Errors
    /// Rejects invalid configuration, bind/hook failures or worker creation failure.
    pub fn start(
        store: DeviceStore,
        config: WorkspaceConfiguration,
        edges: BTreeSet<Edge>,
    ) -> io::Result<Self> {
        config.connections.validate(&store)?;
        let hub = Arc::new(PeerConnectionHub::start(
            store.clone(),
            &config.connections,
        )?);
        let native = NativeInputRuntime::start()?;
        let session = Session::new(store, hub, config, edges, native)?;
        Self::from_session(session)
    }

    fn from_session<B: InputBackend + Send + 'static>(session: Session<B>) -> io::Result<Self> {
        let hub = Arc::clone(&session.hub);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            return_local: AtomicBool::new(false),
            preview_id: AtomicU64::new(0),
            snapshot: Mutex::new(WorkspaceProductSnapshot {
                running: true,
                clean_shutdown: false,
                last_error: None,
                model: session.model(),
            }),
            wake: Condvar::new(),
        });
        let (commands, receiver) = mpsc::sync_channel(16);
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("pierdesk-workspace".into())
            .spawn(move || run(session, &worker_shared, &receiver))?;
        Ok(Self {
            shared,
            commands,
            hub,
            worker: Some(worker),
        })
    }

    /// The clipboard and file services share these authenticated links.
    #[must_use]
    pub fn hub(&self) -> Arc<PeerConnectionHub> {
        Arc::clone(&self.hub)
    }

    #[must_use]
    pub fn snapshot(&self) -> WorkspaceProductSnapshot {
        self.shared
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn pause(&self) {
        self.shared.paused.store(true, Ordering::Release);
        self.shared.wake.notify_all();
    }
    pub fn resume(&self) {
        self.shared.paused.store(false, Ordering::Release);
        self.shared.wake.notify_all();
    }
    pub fn return_local(&self) {
        self.shared.return_local.store(true, Ordering::Release);
        self.shared.wake.notify_all();
    }

    /// Request an exact-map preview; the returned id appears with its rendered model.
    ///
    /// # Errors
    /// Rejects command overload, stopped workers or id exhaustion.
    pub fn preview_placement(&self, peer: DeviceId, direction: PeerPlacement) -> io::Result<u64> {
        let id = self
            .shared
            .preview_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
            .map_err(|_| invalid("layout preview ids exhausted"))?
            + 1;
        self.send(Command::Preview {
            id,
            peer,
            direction,
        })?;
        Ok(id)
    }

    /// Confirm only the preview actually presented to the user.
    ///
    /// # Errors
    /// Rejects command overload or a stopped worker; stale ids appear in `last_error`.
    pub fn confirm_preview(&self, id: u64) -> io::Result<()> {
        self.send(Command::Confirm { id })
    }

    fn send(&self, command: Command) -> io::Result<()> {
        if self.shared.stop.load(Ordering::Acquire) {
            return Err(invalid("workspace worker stopped"));
        }
        self.commands.try_send(command).map_err(io::Error::other)?;
        self.shared.wake.notify_all();
        Ok(())
    }

    /// Join the pump after its route-release and native shutdown barriers.
    ///
    /// # Errors
    /// Reports worker panic or incomplete release instead of claiming clean shutdown.
    pub fn stop(&mut self) -> io::Result<()> {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| io::Error::other("workspace worker panicked"))?;
        }
        self.hub.shutdown();
        let snapshot = self.snapshot();
        if !snapshot.clean_shutdown {
            return Err(io::Error::other(
                snapshot
                    .last_error
                    .unwrap_or_else(|| "workspace shutdown incomplete".into()),
            ));
        }
        Ok(())
    }
}

impl Drop for WorkspaceProductRuntime {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn run<B: InputBackend>(
    mut session: Session<B>,
    shared: &Shared,
    receiver: &mpsc::Receiver<Command>,
) {
    let mut failure = None;
    while !shared.stop.load(Ordering::Acquire) {
        let result = (|| {
            if shared.return_local.swap(false, Ordering::AcqRel) {
                session.input.return_local(&session.hub)?;
            }
            for _ in 0..8 {
                let Ok(command) = receiver.try_recv() else {
                    break;
                };
                let result = session.command(command);
                if let Err(error) = &result
                    && !matches!(
                        error.kind(),
                        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput
                    )
                {
                    return result;
                }
                shared
                    .snapshot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .last_error = result.err().map(|error| error.to_string());
            }
            session.tick(shared.paused.load(Ordering::Acquire))
        })();
        if let Err(error) = result {
            failure = Some(error.to_string());
            break;
        }
        let mut snapshot = shared
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.model = session.model();
        let _ = shared.wake.wait_timeout(snapshot, POLL);
    }
    shared.stop.store(true, Ordering::Release);
    let release = session.input.shutdown(&session.hub);
    let failed = failure.is_some() || release.is_err();
    let mut snapshot = shared
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    snapshot.running = false;
    snapshot.clean_shutdown = release.is_ok();
    snapshot.last_error = release
        .err()
        .map(|error| error.to_string())
        .or(failure)
        .or_else(|| snapshot.last_error.clone());
    snapshot.model = session.model();
    snapshot.model["state"] = if failed { "failed" } else { "stopped" }.into();
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_screen::hub::tests::Network;
    use crate::cross_screen::native_input::NativeInputEvent;
    use crate::cross_screen::{InputEvent, KeyTransition, MouseButton, PhysicalScreenSize};
    use crate::workspace::persistence::{ConnectionSettings, PeerConnectionSettings, save_layout};
    use crate::workspace::{GridPoint, LayoutRevision, edge};
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct BackendState {
        events: Mutex<Vec<NativeInputEvent>>,
        injected: Mutex<Vec<InputEvent>>,
        fullscreen: AtomicBool,
        stopped: AtomicBool,
        fail_release: AtomicBool,
        layout_path: std::path::PathBuf,
        layouts_at_release: Mutex<Vec<Vec<u8>>>,
    }

    struct Backend(Arc<BackendState>);
    impl InputBackend for Backend {
        fn screen(&self) -> PhysicalScreenSize {
            PhysicalScreenSize::new(1920, 1080).unwrap()
        }
        fn fullscreen(&self) -> io::Result<bool> {
            Ok(self.0.fullscreen.load(Ordering::Acquire))
        }
        fn take_events(&self) -> io::Result<Vec<NativeInputEvent>> {
            Ok(std::mem::take(&mut *self.0.events.lock().unwrap()))
        }
        fn inject(&self, events: &[InputEvent]) -> io::Result<()> {
            if events.contains(&button(KeyTransition::Up)) {
                self.0
                    .layouts_at_release
                    .lock()
                    .unwrap()
                    .push(std::fs::read(&self.0.layout_path)?);
            }
            self.0.injected.lock().unwrap().extend_from_slice(events);
            Ok(())
        }
        fn forwarding(&self, _: bool) -> io::Result<()> {
            Ok(())
        }
        fn target_capture(&self, _: bool) -> io::Result<()> {
            Ok(())
        }
        fn release_all(&self) -> io::Result<()> {
            if self.0.fail_release.load(Ordering::Acquire) {
                Err(io::Error::other("injected native release failure"))
            } else {
                Ok(())
            }
        }
        fn shutdown(&mut self) -> io::Result<()> {
            self.0.stopped.store(true, Ordering::Release);
            self.release_all()
        }
    }

    struct Rig {
        runtimes: Vec<WorkspaceProductRuntime>,
        states: Vec<Arc<BackendState>>,
        initial: WorkspaceLayout,
        network: Network,
    }

    impl Rig {
        fn new() -> Self {
            let mut network = Network::new(2);
            network.wait_connected();
            let members = network.ids.iter().copied().collect();
            let edges = BTreeSet::from([edge(network.ids[0], network.ids[1])]);
            let initial = WorkspaceLayout::new(
                [6; 16],
                LayoutRevision {
                    counter: 1,
                    writer: network.ids[0],
                },
                &BTreeMap::from([
                    (network.ids[0], GridPoint { x: 0, y: 0 }),
                    (network.ids[1], GridPoint { x: 1, y: 0 }),
                ]),
                &members,
                &edges,
            )
            .unwrap();
            let mut runtimes = Vec::new();
            let mut states = Vec::new();
            for (index, hub) in std::mem::take(&mut network.hubs).into_iter().enumerate() {
                let state = Arc::new(BackendState {
                    layout_path: network.stores[index].root().join("workspace-layout.v1"),
                    ..BackendState::default()
                });
                save_layout(&network.stores[index], &initial).unwrap();
                let config = WorkspaceConfiguration {
                    connections: ConnectionSettings {
                        schema: 2,
                        local_endpoint: "127.0.0.1:45333".parse().unwrap(),
                        auto_receive: true,
                        peers: hub
                            .snapshots()
                            .iter()
                            .map(|peer| PeerConnectionSettings {
                                peer: peer.peer,
                                last_endpoint: peer.endpoint,
                                manual_endpoint: None,
                                enabled: true,
                            })
                            .collect(),
                    },
                    layout: initial.clone(),
                    behavior: LocalBoundarySettings {
                        schema: 2,
                        enabled: true,
                        corner_guard_enabled: false,
                        corner_guard_pixels: 32,
                        require_second_push: false,
                        pause_on_fullscreen: true,
                    },
                };
                let session = Session::new(
                    network.stores[index].clone(),
                    Arc::new(hub),
                    config,
                    edges.clone(),
                    Backend(Arc::clone(&state)),
                )
                .unwrap();
                runtimes.push(WorkspaceProductRuntime::from_session(session).unwrap());
                states.push(state);
            }
            let rig = Self {
                runtimes,
                states,
                initial,
                network,
            };
            rig.until(|rig| {
                rig.runtimes.iter().all(|runtime| {
                    runtime.snapshot().model["workspace"]["devices"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|device| device["isLocal"] == true || device["edgeReady"] == true)
                })
            });
            rig
        }

        fn until(&self, predicate: impl Fn(&Self) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(6);
            while !predicate(self) {
                assert!(
                    Instant::now() < deadline,
                    "product did not converge: {:?}",
                    self.runtimes
                        .iter()
                        .map(WorkspaceProductRuntime::snapshot)
                        .collect::<Vec<_>>()
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn cross(&self) {
            self.states[0].events.lock().unwrap().extend([
                NativeInputEvent::Physical(button(KeyTransition::Down)),
                NativeInputEvent::Physical(InputEvent::MouseMove { x: 1000, y: 400 }),
                NativeInputEvent::Physical(InputEvent::MouseMove { x: 1919, y: 400 }),
                NativeInputEvent::RawMotion {
                    delta_x: 10,
                    delta_y: 0,
                    message_time_ms: 1,
                },
            ]);
            self.until(|rig| {
                rig.runtimes[0].snapshot().model["state"] == "controlling_peer"
                    && rig.runtimes[1].snapshot().model["state"] == "controlled_by_peer"
            });
        }
    }

    fn button(transition: KeyTransition) -> InputEvent {
        InputEvent::MouseButton {
            button: MouseButton::Left,
            transition,
        }
    }

    #[test]
    fn threaded_product_releases_input_before_durable_layout_commit_and_rejects_stale_preview_id() {
        let mut rig = Rig::new();
        rig.cross();
        let id = rig.runtimes[0]
            .preview_placement(rig.network.ids[1], PeerPlacement::Left)
            .unwrap();
        rig.until(|rig| rig.runtimes[0].snapshot().model["layoutPreview"]["id"] == id);
        rig.runtimes[0].confirm_preview(id + 1).unwrap();
        rig.until(|rig| rig.runtimes[0].snapshot().last_error.is_some());
        assert_eq!(rig.runtimes[0].snapshot().model["workspace"]["revision"], 1);
        rig.runtimes[0].confirm_preview(id).unwrap();
        rig.until(|rig| {
            rig.runtimes
                .iter()
                .all(|runtime| runtime.snapshot().model["workspace"]["revision"] == 2)
        });
        rig.until(|rig| {
            rig.runtimes
                .iter()
                .all(|runtime| runtime.snapshot().model["control"].is_null())
        });
        assert!(
            rig.states[1]
                .layouts_at_release
                .lock()
                .unwrap()
                .contains(&rig.initial.encode().unwrap())
        );
        assert!(
            rig.runtimes[0]
                .hub()
                .snapshots()
                .iter()
                .all(|peer| peer.channel.is_some())
        );
        let retained_hub = rig.runtimes[0].hub();
        for runtime in &mut rig.runtimes {
            runtime.stop().unwrap();
        }
        assert!(
            retained_hub
                .snapshots()
                .iter()
                .all(|peer| peer.channel.is_none())
        );
        assert!(
            rig.states
                .iter()
                .all(|state| state.stopped.load(Ordering::Acquire))
        );
    }

    #[test]
    fn native_product_owner_starts_pass_through_and_joins_before_reporting_shutdown() {
        let mut network = Network::new(1);
        let local = network.ids[0];
        let layout = WorkspaceLayout::new(
            [8; 16],
            LayoutRevision {
                counter: 1,
                writer: local,
            },
            &BTreeMap::from([(local, GridPoint { x: 0, y: 0 })]),
            &BTreeSet::from([local]),
            &BTreeSet::new(),
        )
        .unwrap();
        let config = WorkspaceConfiguration {
            connections: ConnectionSettings {
                schema: 2,
                local_endpoint: "127.0.0.1:45333".parse().unwrap(),
                auto_receive: true,
                peers: vec![],
            },
            layout,
            behavior: LocalBoundarySettings {
                schema: 2,
                enabled: false,
                corner_guard_enabled: false,
                corner_guard_pixels: 32,
                require_second_push: false,
                pause_on_fullscreen: false,
            },
        };
        let native = NativeInputRuntime::start().unwrap();
        assert!(!native.snapshot().forwarding);
        let session = Session::new(
            network.stores[0].clone(),
            Arc::new(network.hubs.remove(0)),
            config,
            BTreeSet::new(),
            native,
        )
        .unwrap();
        let mut runtime = WorkspaceProductRuntime::from_session(session).unwrap();
        assert!(runtime.snapshot().running);
        runtime.stop().unwrap();
        let snapshot = runtime.snapshot();
        assert!(!snapshot.running && snapshot.clean_shutdown);
        assert_eq!(snapshot.model["state"], "stopped");
        let mut next = NativeInputRuntime::start().unwrap();
        assert!(next.stop().unwrap().clean);
    }

    #[test]
    fn fullscreen_pause_retains_links_and_shutdown_failure_is_reported() {
        let mut rig = Rig::new();
        rig.cross();
        rig.states[0].fullscreen.store(true, Ordering::Release);
        rig.until(|rig| {
            rig.runtimes[0].snapshot().model["state"] == "paused_fullscreen"
                && rig.runtimes[1].snapshot().model["control"].is_null()
        });
        assert!(
            rig.runtimes[0]
                .hub()
                .snapshots()
                .iter()
                .all(|peer| peer.channel.is_some())
        );
        rig.states[0].fullscreen.store(false, Ordering::Release);
        rig.until(|rig| rig.runtimes[0].snapshot().model["state"] == "connected");
        rig.states[0].fail_release.store(true, Ordering::Release);
        assert!(rig.runtimes[0].stop().is_err());
        let snapshot = rig.runtimes[0].snapshot();
        assert!(!snapshot.running && !snapshot.clean_shutdown);
        assert!(
            snapshot
                .last_error
                .unwrap()
                .contains("native release failure")
        );
        assert!(rig.states[0].stopped.load(Ordering::Acquire));
    }
}

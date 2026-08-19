#![allow(
    clippy::cast_possible_truncation,
    clippy::needless_pass_by_value,
    clippy::too_many_lines
)]

use std::{
    cell::Cell,
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use tokio::sync::oneshot;
use tracing::{debug, warn};
use x11rb::{
    COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE,
    connection::Connection,
    protocol::{
        Event,
        damage::{ConnectionExt as _, ReportLevel},
        randr::ConnectionExt as _,
        xproto::{
            Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageData, ClientMessageEvent,
            ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, EventMask, ImageFormat,
            InputFocus, MapState, PropMode, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent,
            SelectionRequestEvent, StackMode, Window, WindowClass as XWindowClass,
        },
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};
use xxhash_rust::xxh3::xxh3_64;

use crate::{
    ActionResult, AfterDelivery, Capabilities, ClickRequest, ControllerConfig, ControllerError,
    DesktopController, DragRequest, ErrorCode, FocusWindowRequest, Geometry, ImagePatch, KeyMode,
    KeyRequest, ListWindowsRequest, MonitorInfo, MovePointerRequest, Observation,
    ObservationDelivery, ObservationMetadata, ObserveAfter, ObserveRequest, ObserveTarget,
    PointerInfo, Position, Result, ScreenInfo, ScrollRequest, SecurityInfo, StateGuard, TextMethod,
    TypeTextRequest, WaitCondition, WaitRequest, WaitResult, WindowAction, WindowActionRequest,
    WindowClass, WindowInfo, WindowList,
    capture::{PixelFormat, convert_to_rgb, encode_png},
    keyboard::{KeyStroke, KeyboardMap},
};

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        _NET_ACTIVE_WINDOW,
        _NET_CLIENT_LIST,
        _NET_CLIENT_LIST_STACKING,
        _NET_MOVERESIZE_WINDOW,
        _NET_SUPPORTING_WM_CHECK,
        _NET_WM_DESKTOP,
        _NET_WM_NAME,
        _NET_WM_PID,
        _NET_WM_STATE,
        _NET_WM_STATE_MAXIMIZED_HORZ,
        _NET_WM_STATE_MAXIMIZED_VERT,
        CLIPBOARD,
        TARGETS,
        TEXT,
        UTF8_STRING,
        WM_CHANGE_STATE,
        WM_CLASS,
        WM_DELETE_WINDOW,
        WM_NAME,
        WM_PROTOCOLS,
        X11_MCP_SELECTION,
    }
}

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(1);
const FRAME_HISTORY_SIZE: usize = 64;
const DAMAGE_HISTORY_SIZE: usize = 1_024;
const MAX_DELTA_PATCHES: usize = 16;
const FULL_FRAME_AREA_PERCENT: u64 = 60;

#[derive(Debug, Clone)]
struct FrameRecord {
    frame_id: u64,
    target: ObserveTarget,
    bounds: Geometry,
    desktop_generation: u64,
    signature: u64,
    damage_sequence: u64,
}

#[derive(Debug)]
struct ActionBaseline {
    observation: Option<Observation>,
    damage_sequence: u64,
}

#[derive(Debug, Clone, Copy)]
struct DamageRecord {
    sequence: u64,
    bounds: Geometry,
    topology: bool,
}
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;

#[derive(Clone)]
pub struct ControllerHandle {
    sender: Sender<Envelope>,
}

impl ControllerHandle {
    pub(crate) fn connect(
        config: ControllerConfig,
        emergency_stop: Arc<AtomicBool>,
    ) -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name(format!("x11-controller-{}", config.display))
            .spawn(move || match Actor::new(config, emergency_stop) {
                Ok(mut actor) => {
                    let _ = ready_sender.send(Ok(()));
                    actor.run(&receiver);
                }
                Err(error) => {
                    let _ = ready_sender.send(Err(error));
                }
            })
            .map_err(|error| ControllerError::new(ErrorCode::Internal, error.to_string()))?;
        ready_receiver.recv().map_err(|_| {
            ControllerError::new(ErrorCode::Internal, "X11 actor failed to start")
        })??;
        Ok(Self { sender })
    }

    #[doc(hidden)]
    pub async fn capture_count(&self) -> Result<u64> {
        match self.request(Operation::CaptureCount).await? {
            Response::U64(value) => Ok(value),
            _ => Err(unexpected_response()),
        }
    }

    async fn request(&self, operation: Operation) -> Result<Response> {
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(Envelope {
                operation,
                response,
            })
            .map_err(|_| ControllerError::new(ErrorCode::Internal, "X11 actor stopped"))?;
        receiver
            .await
            .map_err(|_| ControllerError::new(ErrorCode::Internal, "X11 actor dropped response"))?
    }
}

#[async_trait]
impl DesktopController for ControllerHandle {
    async fn capabilities(&self) -> Result<Capabilities> {
        match self.request(Operation::Capabilities).await? {
            Response::Capabilities(value) => Ok(value),
            _ => Err(unexpected_response()),
        }
    }

    async fn observe(&self, request: ObserveRequest) -> Result<Observation> {
        match self.request(Operation::Observe(request)).await? {
            Response::Observation(value) => Ok(value),
            _ => Err(unexpected_response()),
        }
    }

    async fn list_windows(&self, request: ListWindowsRequest) -> Result<WindowList> {
        match self.request(Operation::ListWindows(request)).await? {
            Response::Windows(value) => Ok(value),
            _ => Err(unexpected_response()),
        }
    }

    async fn focus_window(&self, request: FocusWindowRequest) -> Result<ActionResult> {
        self.action(Operation::Focus(request)).await
    }

    async fn move_pointer(&self, request: MovePointerRequest) -> Result<ActionResult> {
        self.action(Operation::MovePointer(request)).await
    }

    async fn click(&self, request: ClickRequest) -> Result<ActionResult> {
        self.action(Operation::Click(request)).await
    }

    async fn drag(&self, request: DragRequest) -> Result<ActionResult> {
        self.action(Operation::Drag(request)).await
    }

    async fn scroll(&self, request: ScrollRequest) -> Result<ActionResult> {
        self.action(Operation::Scroll(request)).await
    }

    async fn key(&self, request: KeyRequest) -> Result<ActionResult> {
        self.action(Operation::Key(request)).await
    }

    async fn type_text(&self, request: TypeTextRequest) -> Result<ActionResult> {
        self.action(Operation::TypeText(request)).await
    }

    async fn window_action(&self, request: WindowActionRequest) -> Result<ActionResult> {
        self.action(Operation::WindowAction(request)).await
    }

    async fn wait_for(&self, request: WaitRequest) -> Result<WaitResult> {
        match self.request(Operation::Wait(request)).await? {
            Response::Wait(value) => Ok(value),
            _ => Err(unexpected_response()),
        }
    }

    async fn validate_state_guard(
        &self,
        guard: StateGuard,
        require_frame: bool,
        include_current_pointer: bool,
        positions: Vec<Position>,
    ) -> Result<()> {
        match self
            .request(Operation::ValidateStateGuard {
                guard,
                require_frame,
                include_current_pointer,
                positions,
            })
            .await?
        {
            Response::Unit => Ok(()),
            _ => Err(unexpected_response()),
        }
    }

    async fn validate_window_allowed(&self, window_ref: String) -> Result<()> {
        match self
            .request(Operation::ValidateWindowAllowed(window_ref))
            .await?
        {
            Response::Unit => Ok(()),
            _ => Err(unexpected_response()),
        }
    }

    async fn release_input(&self) -> Result<()> {
        match self.request(Operation::ReleaseInput).await? {
            Response::Unit => Ok(()),
            _ => Err(unexpected_response()),
        }
    }
}

impl ControllerHandle {
    async fn action(&self, operation: Operation) -> Result<ActionResult> {
        match self.request(operation).await? {
            Response::Action(value) => Ok(value),
            _ => Err(unexpected_response()),
        }
    }
}

fn unexpected_response() -> ControllerError {
    ControllerError::new(
        ErrorCode::Internal,
        "X11 actor returned an unexpected response",
    )
}

struct Envelope {
    operation: Operation,
    response: oneshot::Sender<Result<Response>>,
}

enum Operation {
    Capabilities,
    Observe(ObserveRequest),
    ListWindows(ListWindowsRequest),
    Focus(FocusWindowRequest),
    MovePointer(MovePointerRequest),
    Click(ClickRequest),
    Drag(DragRequest),
    Scroll(ScrollRequest),
    Key(KeyRequest),
    TypeText(TypeTextRequest),
    WindowAction(WindowActionRequest),
    Wait(WaitRequest),
    ValidateStateGuard {
        guard: StateGuard,
        require_frame: bool,
        include_current_pointer: bool,
        positions: Vec<Position>,
    },
    ValidateWindowAllowed(String),
    ReleaseInput,
    CaptureCount,
}

enum Response {
    Capabilities(Capabilities),
    Observation(Observation),
    Windows(WindowList),
    Action(ActionResult),
    Wait(WaitResult),
    Unit,
    U64(u64),
}

struct Actor {
    connection: RustConnection,
    screen_number: usize,
    root: Window,
    atoms: Atoms,
    config: ControllerConfig,
    capabilities: Capabilities,
    allowlist: Vec<GlobMatcher>,
    emergency_stop: Arc<AtomicBool>,
    rate_limiter: RateLimiter,
    window_refs: HashMap<Window, String>,
    observed_windows: HashSet<Window>,
    ref_xids: HashMap<String, Window>,
    next_window_ref: u64,
    frame_id: u64,
    desktop_generation: u64,
    last_topology_signature: Option<u64>,
    frame_history: VecDeque<FrameRecord>,
    damage: Option<u32>,
    damage_sequence: u64,
    damage_history: VecDeque<DamageRecord>,
    held_keys: HashSet<u8>,
    held_buttons: HashSet<u8>,
    clipboard_window: Window,
    clipboard_content: Option<Vec<u8>>,
    capture_calls: Cell<u64>,
}

impl Actor {
    fn new(config: ControllerConfig, emergency_stop: Arc<AtomicBool>) -> Result<Self> {
        let (connection, screen_number) = x11rb::connect(Some(&config.display))
            .map_err(|error| ControllerError::x11("connect to display", error))?;
        let screen = connection
            .setup()
            .roots
            .get(screen_number)
            .cloned()
            .ok_or_else(|| ControllerError::new(ErrorCode::X11, "X11 screen is missing"))?;
        let atoms = Atoms::new(&connection)
            .map_err(|error| ControllerError::x11("intern atoms", error))?
            .reply()
            .map_err(|error| ControllerError::x11("intern atoms", error))?;

        connection
            .change_window_attributes(
                screen.root,
                &ChangeWindowAttributesAux::new()
                    .event_mask(EventMask::SUBSTRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
            )
            .map_err(|error| ControllerError::x11("subscribe to root events", error))?
            .check()
            .map_err(|error| ControllerError::x11("subscribe to root events", error))?;

        let clipboard_window = connection
            .generate_id()
            .map_err(|error| ControllerError::x11("allocate clipboard window", error))?;
        connection
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                clipboard_window,
                screen.root,
                0,
                0,
                1,
                1,
                0,
                XWindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .map_err(|error| ControllerError::x11("create clipboard window", error))?
            .check()
            .map_err(|error| ControllerError::x11("create clipboard window", error))?;

        let mut extensions = probe_extensions(&connection)?;
        let damage = if extensions.get("damage") == Some(&true) {
            let initialized = (|| -> Result<u32> {
                connection
                    .damage_query_version(1, 1)
                    .map_err(|error| ControllerError::x11("query Damage version", error))?
                    .reply()
                    .map_err(|error| ControllerError::x11("query Damage version", error))?;
                let id = connection
                    .generate_id()
                    .map_err(|error| ControllerError::x11("allocate Damage object", error))?;
                connection
                    .damage_create(id, screen.root, ReportLevel::RAW_RECTANGLES)
                    .map_err(|error| ControllerError::x11("create Damage object", error))?
                    .check()
                    .map_err(|error| ControllerError::x11("create Damage object", error))?;
                Ok(id)
            })();
            match initialized {
                Ok(id) => Some(id),
                Err(error) => {
                    warn!(%error, "DAMAGE advertised but could not be initialized; using capture polling");
                    extensions.insert("damage".to_owned(), false);
                    None
                }
            }
        } else {
            None
        };
        let ewmh =
            property_u32(&connection, screen.root, atoms._NET_SUPPORTING_WM_CHECK)?.is_some();
        let monitors = query_monitors(&connection, screen.root, &screen, &extensions);
        let capabilities = Capabilities {
            display: config.display.clone(),
            screen: ScreenInfo {
                width: screen.width_in_pixels,
                height: screen.height_in_pixels,
                depth: screen.root_depth,
            },
            monitors,
            extensions,
            ewmh,
            security: SecurityInfo {
                window_allowlist_enabled: !config.allow_window_classes.is_empty(),
                input_events_per_second: config.max_input_events_per_second,
            },
        };
        let allowlist = config
            .allow_window_classes
            .iter()
            .map(|pattern| {
                Glob::new(pattern)
                    .map(|glob| glob.compile_matcher())
                    .map_err(|error| {
                        ControllerError::new(
                            ErrorCode::InvalidInput,
                            format!("invalid window-class glob {pattern:?}: {error}"),
                        )
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        connection
            .flush()
            .map_err(|error| ControllerError::x11("flush initialization", error))?;

        Ok(Self {
            connection,
            screen_number,
            root: screen.root,
            atoms,
            rate_limiter: RateLimiter::new(config.max_input_events_per_second),
            config,
            capabilities,
            allowlist,
            emergency_stop,
            window_refs: HashMap::new(),
            observed_windows: HashSet::new(),
            ref_xids: HashMap::new(),
            next_window_ref: 1,
            frame_id: 0,
            desktop_generation: 1,
            last_topology_signature: None,
            frame_history: VecDeque::new(),
            damage,
            damage_sequence: 0,
            damage_history: VecDeque::new(),
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
            clipboard_window,
            clipboard_content: None,
            capture_calls: Cell::new(0),
        })
    }

    fn run(&mut self, receiver: &Receiver<Envelope>) {
        loop {
            match receiver.recv_timeout(Duration::from_millis(10)) {
                Ok(envelope) => {
                    let result = self.handle(envelope.operation);
                    let _ = envelope.response.send(result);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if let Err(error) = self.process_pending_events() {
                warn!(%error, "failed to process X11 event");
            }
        }
        self.release_held_input();
        if let Some(damage) = self.damage {
            let _ = self.connection.damage_destroy(damage);
        }
        let _ = self.connection.destroy_window(self.clipboard_window);
        let _ = self.connection.flush();
    }

    fn handle(&mut self, operation: Operation) -> Result<Response> {
        let result = match operation {
            Operation::Capabilities => Ok(Response::Capabilities(self.capabilities.clone())),
            Operation::Observe(request) => self.observe(request).map(Response::Observation),
            Operation::ListWindows(request) => self.list_windows(request).map(Response::Windows),
            Operation::Focus(request) => self.focus(request).map(Response::Action),
            Operation::MovePointer(request) => self.move_pointer(request).map(Response::Action),
            Operation::Click(request) => self.click(request).map(Response::Action),
            Operation::Drag(request) => self.drag(request).map(Response::Action),
            Operation::Scroll(request) => self.scroll(request).map(Response::Action),
            Operation::Key(request) => self.key(request).map(Response::Action),
            Operation::TypeText(request) => self.type_text(request).map(Response::Action),
            Operation::WindowAction(request) => self.window_action(request).map(Response::Action),
            Operation::Wait(request) => self.wait_for(request).map(Response::Wait),
            Operation::ValidateStateGuard {
                guard,
                require_frame,
                include_current_pointer,
                positions,
            } => {
                let positions = positions.iter().collect::<Vec<_>>();
                self.validate_guard(&guard, require_frame, include_current_pointer, &positions)
                    .map(|()| Response::Unit)
            }
            Operation::ValidateWindowAllowed(window_ref) => self
                .resolve_window(&window_ref)
                .and_then(|window| self.ensure_window_allowed(&window))
                .map(|()| Response::Unit),
            Operation::ReleaseInput => {
                self.release_held_input();
                Ok(Response::Unit)
            }
            Operation::CaptureCount => Ok(Response::U64(self.capture_calls.get())),
        };
        if result.is_err() {
            self.release_held_input();
        }
        result
    }

    fn list_windows(&mut self, request: ListWindowsRequest) -> Result<WindowList> {
        let mut list = self.window_list()?;
        if !request.include_unmapped {
            list.windows.retain(|window| window.mapped);
        }
        if let Some(selector) = request.selector {
            list.windows.retain(|window| selector.matches(window));
        }
        Ok(list)
    }

    fn window_list(&mut self) -> Result<WindowList> {
        let xids = property_u32_list(
            &self.connection,
            self.root,
            self.atoms._NET_CLIENT_LIST_STACKING,
        )?
        .or_else(|| {
            property_u32_list(&self.connection, self.root, self.atoms._NET_CLIENT_LIST)
                .ok()
                .flatten()
        })
        .unwrap_or_else(|| {
            self.connection
                .query_tree(self.root)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map_or_else(Vec::new, |reply| reply.children)
        });

        let current: HashSet<Window> = xids.iter().copied().collect();
        self.window_refs.retain(|xid, _| current.contains(xid));
        let mut windows = Vec::new();
        for xid in xids {
            if let Ok(window) = self.window_info(xid) {
                windows.push(window);
            }
        }
        let active_xid = property_u32(&self.connection, self.root, self.atoms._NET_ACTIVE_WINDOW)?;
        let active_window = active_xid.and_then(|xid| self.window_refs.get(&xid).cloned());
        self.update_topology_generation(&windows, active_window.as_deref());
        Ok(WindowList {
            desktop_generation: self.desktop_generation,
            active_window,
            windows,
        })
    }

    fn window_info(&mut self, xid: Window) -> Result<WindowInfo> {
        if self.observed_windows.insert(xid) {
            self.connection
                .change_window_attributes(
                    xid,
                    &ChangeWindowAttributesAux::new()
                        .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
                )
                .map_err(|error| ControllerError::x11("subscribe to window events", error))?
                .check()
                .map_err(|error| ControllerError::x11("subscribe to window events", error))?;
        }
        let attributes = self
            .connection
            .get_window_attributes(xid)
            .map_err(|error| ControllerError::x11("request window attributes", error))?
            .reply()
            .map_err(|error| ControllerError::x11("read window attributes", error))?;
        let geometry = self
            .connection
            .get_geometry(xid)
            .map_err(|error| ControllerError::x11("request window geometry", error))?
            .reply()
            .map_err(|error| ControllerError::x11("read window geometry", error))?;
        let translated = self
            .connection
            .translate_coordinates(xid, self.root, 0, 0)
            .map_err(|error| ControllerError::x11("translate window geometry", error))?
            .reply()
            .map_err(|error| ControllerError::x11("translate window geometry", error))?;
        let title = property_bytes(&self.connection, xid, self.atoms._NET_WM_NAME)?
            .or_else(|| {
                property_bytes(&self.connection, xid, self.atoms.WM_NAME)
                    .ok()
                    .flatten()
            })
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let class_bytes = property_bytes(&self.connection, xid, self.atoms.WM_CLASS)?;
        let (instance, name) = class_bytes.map_or((None, None), |bytes| {
            let mut parts = bytes
                .split(|byte| *byte == 0)
                .filter(|value| !value.is_empty())
                .map(|value| String::from_utf8_lossy(value).into_owned());
            (parts.next(), parts.next())
        });
        let window_ref = if let Some(existing) = self.window_refs.get(&xid) {
            existing.clone()
        } else {
            let handle = format!("window-{}", self.next_window_ref);
            self.next_window_ref += 1;
            self.window_refs.insert(xid, handle.clone());
            self.ref_xids.insert(handle.clone(), xid);
            handle
        };
        Ok(WindowInfo {
            window_ref,
            xid: format!("0x{xid:08x}"),
            title,
            class: WindowClass { instance, name },
            pid: property_u32(&self.connection, xid, self.atoms._NET_WM_PID)?,
            desktop: property_u32(&self.connection, xid, self.atoms._NET_WM_DESKTOP)?,
            geometry: Geometry {
                x: i32::from(translated.dst_x),
                y: i32::from(translated.dst_y),
                width: u32::from(geometry.width),
                height: u32::from(geometry.height),
            },
            mapped: attributes.map_state == MapState::VIEWABLE,
        })
    }

    fn update_topology_generation(&mut self, windows: &[WindowInfo], active: Option<&str>) {
        let signature = serde_json::to_vec(&(windows, active)).map_or(0, |bytes| xxh3_64(&bytes));
        if self
            .last_topology_signature
            .is_some_and(|previous| previous != signature)
        {
            self.desktop_generation = self.desktop_generation.saturating_add(1);
        }
        self.last_topology_signature = Some(signature);
    }

    fn observe(&mut self, request: ObserveRequest) -> Result<Observation> {
        self.process_pending_events()?;
        let window_list = self.window_list()?;
        let bounds = self.resolve_observe_target(&request.target)?;
        let requested_delivery = request.delivery.clone();
        let mut base_frame_id = None;
        let (delivery, complete, patches, images, signature) = match requested_delivery {
            ObservationDelivery::Full => {
                let (rgb, png) = self.capture(bounds)?;
                let mut signature_bytes = rgb;
                signature_bytes.extend_from_slice(&self.desktop_generation.to_ne_bytes());
                (
                    ObservationDelivery::Full,
                    true,
                    vec![ImagePatch {
                        bounds,
                        image_index: 0,
                    }],
                    vec![png],
                    xxh3_64(&signature_bytes),
                )
            }
            ObservationDelivery::Delta { since_frame_id } => {
                let base = self
                    .frame_history
                    .iter()
                    .find(|frame| frame.frame_id == since_frame_id)
                    .cloned()
                    .ok_or_else(|| {
                        ControllerError::new(
                            ErrorCode::StaleFrame,
                            format!("frame {since_frame_id} is not in the 64-frame history"),
                        )
                        .retryable(true)
                    })?;
                base_frame_id = Some(since_frame_id);
                if base.target != request.target {
                    return Err(ControllerError::new(
                        ErrorCode::StaleFrame,
                        "delta target does not match the base frame target",
                    )
                    .retryable(true));
                }
                if self.damage.is_none() {
                    let (rgb, png) = self.capture(bounds)?;
                    let mut signature_bytes = rgb;
                    signature_bytes.extend_from_slice(&self.desktop_generation.to_ne_bytes());
                    let signature = xxh3_64(&signature_bytes);
                    if base.desktop_generation != self.desktop_generation
                        || base.bounds != bounds
                        || base.signature != signature
                    {
                        (
                            ObservationDelivery::Full,
                            true,
                            vec![ImagePatch {
                                bounds,
                                image_index: 0,
                            }],
                            vec![png],
                            signature,
                        )
                    } else {
                        (
                            ObservationDelivery::Delta { since_frame_id },
                            false,
                            Vec::new(),
                            Vec::new(),
                            signature,
                        )
                    }
                } else {
                    let journal_expired = self.damage_history.front().is_some_and(|record| {
                        record.sequence > base.damage_sequence.saturating_add(1)
                            && self.damage_sequence > base.damage_sequence
                    });
                    if journal_expired {
                        return Err(ControllerError::new(
                            ErrorCode::StaleFrame,
                            "damage history for the base frame has expired",
                        )
                        .retryable(true));
                    }
                    let damage = self
                        .damage_history
                        .iter()
                        .filter(|record| record.sequence > base.damage_sequence)
                        .copied()
                        .collect::<Vec<_>>();
                    let force_full = base.desktop_generation != self.desktop_generation
                        || base.bounds != bounds
                        || damage.iter().any(|record| record.topology);
                    let mut changed = damage
                        .iter()
                        .filter_map(|record| intersect_geometry(record.bounds, bounds))
                        .collect::<Vec<_>>();
                    coalesce_rectangles(&mut changed);
                    let inefficient = delta_should_use_full(&changed, bounds);
                    if force_full || inefficient {
                        let (rgb, png) = self.capture(bounds)?;
                        let mut signature_bytes = rgb;
                        signature_bytes.extend_from_slice(&self.desktop_generation.to_ne_bytes());
                        (
                            ObservationDelivery::Full,
                            true,
                            vec![ImagePatch {
                                bounds,
                                image_index: 0,
                            }],
                            vec![png],
                            xxh3_64(&signature_bytes),
                        )
                    } else {
                        let mut patches = Vec::with_capacity(changed.len());
                        let mut images = Vec::with_capacity(changed.len());
                        for patch_bounds in changed {
                            let (_, png) = self.capture(patch_bounds)?;
                            let image_index = images.len();
                            images.push(png);
                            patches.push(ImagePatch {
                                bounds: patch_bounds,
                                image_index,
                            });
                        }
                        (
                            ObservationDelivery::Delta { since_frame_id },
                            false,
                            patches,
                            images,
                            self.damage_sequence ^ self.desktop_generation.rotate_left(32),
                        )
                    }
                }
            }
        };

        let pointer = self.pointer_position()?;
        self.frame_id = self.frame_id.saturating_add(1);
        self.frame_history.push_back(FrameRecord {
            frame_id: self.frame_id,
            target: request.target,
            bounds,
            desktop_generation: self.desktop_generation,
            signature,
            damage_sequence: self.damage_sequence,
        });
        while self.frame_history.len() > FRAME_HISTORY_SIZE {
            self.frame_history.pop_front();
        }
        Ok(Observation {
            metadata: ObservationMetadata {
                frame_id: self.frame_id,
                desktop_generation: self.desktop_generation,
                bounds,
                pointer,
                active_window: window_list.active_window,
                windows: if request.include_windows {
                    window_list.windows
                } else {
                    Vec::new()
                },
            },
            delivery,
            base_frame_id,
            complete,
            patches,
            images,
            signature,
        })
    }

    fn resolve_observe_target(&mut self, target: &ObserveTarget) -> Result<Geometry> {
        let screen_width = u32::from(self.capabilities.screen.width);
        let screen_height = u32::from(self.capabilities.screen.height);
        match target {
            ObserveTarget::Desktop => Ok(Geometry {
                x: 0,
                y: 0,
                width: screen_width,
                height: screen_height,
            }),
            ObserveTarget::Window { window_ref } => {
                let window = self.resolve_window(window_ref)?;
                clip_geometry(window.geometry, screen_width, screen_height)
            }
            ObserveTarget::Region {
                x,
                y,
                width,
                height,
            } => clip_geometry(
                Geometry {
                    x: *x,
                    y: *y,
                    width: *width,
                    height: *height,
                },
                screen_width,
                screen_height,
            ),
        }
    }

    fn capture(&self, bounds: Geometry) -> Result<(Vec<u8>, Vec<u8>)> {
        self.capture_calls
            .set(self.capture_calls.get().saturating_add(1));
        let x = i16::try_from(bounds.x).map_err(|_| {
            ControllerError::new(
                ErrorCode::InvalidInput,
                "capture x coordinate is out of range",
            )
        })?;
        let y = i16::try_from(bounds.y).map_err(|_| {
            ControllerError::new(
                ErrorCode::InvalidInput,
                "capture y coordinate is out of range",
            )
        })?;
        let width = u16::try_from(bounds.width).map_err(|_| {
            ControllerError::new(ErrorCode::InvalidInput, "capture width is out of range")
        })?;
        let height = u16::try_from(bounds.height).map_err(|_| {
            ControllerError::new(ErrorCode::InvalidInput, "capture height is out of range")
        })?;
        let reply = self
            .connection
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                x,
                y,
                width,
                height,
                u32::MAX,
            )
            .map_err(|error| ControllerError::x11("request screenshot", error))?
            .reply()
            .map_err(|error| ControllerError::x11("capture screenshot", error))?;
        let setup = self.connection.setup();
        let pixmap_format = setup
            .pixmap_formats
            .iter()
            .find(|format| format.depth == reply.depth)
            .ok_or_else(|| {
                ControllerError::new(
                    ErrorCode::UnsupportedCapability,
                    format!("no pixel format for depth {}", reply.depth),
                )
            })?;
        let screen = &setup.roots[self.screen_number];
        let visual = screen
            .allowed_depths
            .iter()
            .find(|depth| depth.depth == reply.depth)
            .and_then(|depth| {
                depth
                    .visuals
                    .iter()
                    .find(|visual| visual.visual_id == screen.root_visual)
                    .or_else(|| depth.visuals.first())
            })
            .ok_or_else(|| {
                ControllerError::new(
                    ErrorCode::UnsupportedCapability,
                    format!("no visual for depth {}", reply.depth),
                )
            })?;
        let rgb = convert_to_rgb(
            &reply.data,
            bounds.width,
            bounds.height,
            PixelFormat {
                bits_per_pixel: pixmap_format.bits_per_pixel,
                scanline_pad: pixmap_format.scanline_pad,
                byte_order: setup.image_byte_order,
                red_mask: visual.red_mask,
                green_mask: visual.green_mask,
                blue_mask: visual.blue_mask,
            },
        )?;
        let png = encode_png(&rgb, bounds.width, bounds.height)?;
        Ok((rgb, png))
    }

    fn pointer_position(&self) -> Result<PointerInfo> {
        let pointer = self
            .connection
            .query_pointer(self.root)
            .map_err(|error| ControllerError::x11("request pointer position", error))?
            .reply()
            .map_err(|error| ControllerError::x11("read pointer position", error))?;
        Ok(PointerInfo {
            x: i32::from(pointer.root_x),
            y: i32::from(pointer.root_y),
        })
    }

    fn resolve_window(&mut self, window_ref: &str) -> Result<WindowInfo> {
        let xid = *self.ref_xids.get(window_ref).ok_or_else(|| {
            ControllerError::new(
                ErrorCode::StaleWindow,
                format!("unknown or stale window reference: {window_ref}"),
            )
        })?;
        let info = self.window_info(xid).map_err(|_| {
            ControllerError::new(
                ErrorCode::StaleWindow,
                format!("window reference is stale: {window_ref}"),
            )
        })?;
        if info.window_ref != window_ref {
            return Err(ControllerError::new(
                ErrorCode::StaleWindow,
                format!("window reference is stale: {window_ref}"),
            ));
        }
        Ok(info)
    }
}

impl Actor {
    fn ensure_mutation_allowed(&self) -> Result<()> {
        if self.emergency_stop.load(Ordering::SeqCst) {
            return Err(ControllerError::new(
                ErrorCode::EmergencyStop,
                "input is disabled because the emergency stop is latched",
            ));
        }
        Ok(())
    }

    fn ensure_xtest(&self) -> Result<()> {
        self.ensure_mutation_allowed()?;
        if self.capabilities.extensions.get("xtest") != Some(&true) {
            return Err(ControllerError::new(
                ErrorCode::UnsupportedCapability,
                "the XTEST extension is unavailable on this display",
            ));
        }
        Ok(())
    }

    fn validate_guard(
        &mut self,
        guard: &StateGuard,
        require_frame: bool,
        include_current_pointer: bool,
        positions: &[&Position],
    ) -> Result<()> {
        if guard.prevalidated {
            return Ok(());
        }
        self.process_pending_events()?;
        let windows = self.window_list()?;
        if let Some(expected) = &guard.expected_active_window
            && windows.active_window.as_deref() != Some(expected.as_str())
        {
            return Err(ControllerError::new(
                ErrorCode::PreconditionFailed,
                "the active window changed since the action was planned",
            )
            .retryable(true)
            .with_details(serde_json::json!({
                "expected_active_window": expected,
                "active_window": windows.active_window,
            })));
        }
        let Some(frame_id) = guard.frame_id else {
            if require_frame {
                return Err(ControllerError::new(
                    ErrorCode::PreconditionFailed,
                    "this targeted action requires guard.frame_id",
                )
                .retryable(true));
            }
            return Ok(());
        };
        let frame = self
            .frame_history
            .iter()
            .find(|frame| frame.frame_id == frame_id)
            .cloned()
            .ok_or_else(|| {
                ControllerError::new(
                    ErrorCode::StaleFrame,
                    format!("frame {frame_id} is not in the 64-frame history"),
                )
                .retryable(true)
            })?;
        if frame.desktop_generation != self.desktop_generation {
            return Err(ControllerError::new(
                ErrorCode::PreconditionFailed,
                "desktop topology changed since the guarded frame",
            )
            .retryable(true));
        }
        if self.damage_history.front().is_some_and(|record| {
            record.sequence > frame.damage_sequence.saturating_add(1)
                && self.damage_sequence > frame.damage_sequence
        }) {
            return Err(ControllerError::new(
                ErrorCode::StaleFrame,
                "damage history for the guarded frame has expired",
            )
            .retryable(true));
        }
        if self
            .damage_history
            .iter()
            .filter(|record| record.sequence > frame.damage_sequence)
            .any(|record| {
                record.topology || intersect_geometry(record.bounds, frame.bounds).is_some()
            })
        {
            return Err(ControllerError::new(
                ErrorCode::PreconditionFailed,
                "the guarded pixels changed before the action",
            )
            .retryable(true));
        }
        if self.damage.is_none() {
            let (rgb, _) = self.capture(frame.bounds)?;
            let mut signature_bytes = rgb;
            signature_bytes.extend_from_slice(&self.desktop_generation.to_ne_bytes());
            if xxh3_64(&signature_bytes) != frame.signature {
                return Err(ControllerError::new(
                    ErrorCode::PreconditionFailed,
                    "the guarded pixels changed before the action",
                )
                .retryable(true));
            }
        }
        let pointer_position = if include_current_pointer {
            let pointer = self.pointer_position()?;
            Some((pointer.x, pointer.y))
        } else {
            None
        };
        for resolved in positions
            .iter()
            .map(|position| self.resolve_position(position))
            .chain(pointer_position.into_iter().map(Ok))
        {
            let (x, y) = resolved?;
            if intersect_geometry(
                Geometry {
                    x,
                    y,
                    width: 1,
                    height: 1,
                },
                frame.bounds,
            )
            .is_none()
            {
                return Err(ControllerError::new(
                    ErrorCode::PreconditionFailed,
                    "the guarded frame does not cover the action target",
                )
                .retryable(true));
            }
        }
        Ok(())
    }

    fn action_baseline(&mut self, after: Option<&ObserveAfter>) -> Result<ActionBaseline> {
        self.ensure_mutation_allowed()?;
        self.process_pending_events()?;
        let damage_sequence = self.damage_sequence;
        let observation = after
            .filter(|after| self.damage.is_none() || matches!(after.delivery, AfterDelivery::Delta))
            .map(|after| {
                self.observe(ObserveRequest {
                    target: after.target.clone(),
                    include_windows: after.include_windows,
                    delivery: ObservationDelivery::Full,
                })
            })
            .transpose()?;
        Ok(ActionBaseline {
            observation,
            damage_sequence,
        })
    }

    fn finish_action(
        &mut self,
        after: Option<&ObserveAfter>,
        baseline: &ActionBaseline,
        warnings: Vec<String>,
    ) -> Result<ActionResult> {
        let Some(after) = after else {
            return Ok(ActionResult {
                ok: true,
                settled: true,
                observation: None,
                warnings,
            });
        };
        let (observation, settled) = self.settle(after, baseline)?;
        Ok(ActionResult {
            ok: true,
            settled,
            observation: Some(observation),
            warnings,
        })
    }

    fn settle(
        &mut self,
        after: &ObserveAfter,
        baseline: &ActionBaseline,
    ) -> Result<(Observation, bool)> {
        if after.quiet_ms > after.timeout_ms {
            return Err(ControllerError::new(
                ErrorCode::InvalidInput,
                "observe_after.quiet_ms cannot exceed timeout_ms",
            ));
        }
        let started = Instant::now();
        let timeout = Duration::from_millis(after.timeout_ms);
        let quiet = Duration::from_millis(after.quiet_ms);

        if self.damage.is_some() {
            let mut last_sequence = baseline.damage_sequence;
            let mut change_seen = !after.require_change;
            let mut quiet_since = Instant::now();
            let settled = loop {
                self.process_pending_events()?;
                if self.damage_sequence != last_sequence {
                    last_sequence = self.damage_sequence;
                    quiet_since = Instant::now();
                    change_seen = true;
                }
                if change_seen && quiet_since.elapsed() >= quiet {
                    break true;
                }
                if started.elapsed() >= timeout {
                    break false;
                }
                let wake_at = (quiet_since + quiet).min(started + timeout);
                self.wait_for_x11_event_until(wake_at)?;
            };
            let delivery =
                match after.delivery {
                    AfterDelivery::Full => ObservationDelivery::Full,
                    AfterDelivery::Delta => baseline.observation.as_ref().map_or(
                        ObservationDelivery::Full,
                        |observation| ObservationDelivery::Delta {
                            since_frame_id: observation.metadata.frame_id,
                        },
                    ),
                };
            let observation = self.observe(ObserveRequest {
                target: after.target.clone(),
                include_windows: after.include_windows,
                delivery,
            })?;
            return Ok((observation, settled));
        }

        let baseline_signature = baseline.observation.as_ref().map(|value| value.signature);
        let mut latest = self.observe(ObserveRequest {
            target: after.target.clone(),
            include_windows: after.include_windows,
            delivery: ObservationDelivery::Full,
        })?;
        let mut change_seen = !after.require_change
            || baseline_signature.is_none_or(|signature| signature != latest.signature);
        let mut quiet_since = Instant::now();
        let mut last_signature = latest.signature;
        loop {
            if change_seen && quiet_since.elapsed() >= quiet {
                return Ok((latest, true));
            }
            if started.elapsed() >= timeout {
                return Ok((latest, false));
            }
            thread::sleep(POLL_INTERVAL.min(timeout.saturating_sub(started.elapsed())));
            self.process_pending_events()?;
            let next = self.observe(ObserveRequest {
                target: after.target.clone(),
                include_windows: after.include_windows,
                delivery: ObservationDelivery::Full,
            })?;
            if next.signature != last_signature {
                quiet_since = Instant::now();
                change_seen = true;
                last_signature = next.signature;
            }
            latest = next;
        }
    }

    fn focus(&mut self, request: FocusWindowRequest) -> Result<ActionResult> {
        self.validate_guard(&request.guard, false, false, &[])?;
        let baseline = self.action_baseline(request.observe_after.as_ref())?;
        let window = self.resolve_window(&request.window_ref)?;
        self.ensure_window_allowed(&window)?;
        let xid = self.xid_for_ref(&request.window_ref)?;
        if self.capabilities.ewmh {
            self.send_root_message(
                xid,
                self.atoms._NET_ACTIVE_WINDOW,
                [1, CURRENT_TIME, 0, 0, 0],
            )?;
        } else {
            self.connection
                .set_input_focus(InputFocus::PARENT, xid, CURRENT_TIME)
                .map_err(|error| ControllerError::x11("focus window", error))?
                .check()
                .map_err(|error| ControllerError::x11("focus window", error))?;
        }
        self.connection
            .flush()
            .map_err(|error| ControllerError::x11("flush focus", error))?;
        self.finish_action(request.observe_after.as_ref(), &baseline, Vec::new())
    }

    fn move_pointer(&mut self, request: MovePointerRequest) -> Result<ActionResult> {
        self.ensure_xtest()?;
        let guarded = matches!(
            request.position,
            Position::Window { .. } | Position::WindowRelative { .. }
        );
        self.validate_guard(&request.guard, guarded, false, &[&request.position])?;
        let baseline = self.action_baseline(request.observe_after.as_ref())?;
        self.rate_limiter.consume(1)?;
        let (x, y) = self.resolve_position(&request.position)?;
        self.fake_input(MOTION_NOTIFY, 0, x, y)?;
        self.connection
            .flush()
            .map_err(|error| ControllerError::x11("flush pointer move", error))?;
        self.finish_action(request.observe_after.as_ref(), &baseline, Vec::new())
    }

    fn click(&mut self, request: ClickRequest) -> Result<ActionResult> {
        self.ensure_xtest()?;
        let positions = request
            .position
            .as_ref()
            .map_or_else(Vec::new, |position| vec![position]);
        self.validate_guard(&request.guard, true, request.position.is_none(), &positions)?;
        if request.button == 0 || request.count == 0 || request.count > 10 {
            return Err(ControllerError::new(
                ErrorCode::InvalidInput,
                "button must be non-zero and count must be between 1 and 10",
            ));
        }
        let baseline = self.action_baseline(request.observe_after.as_ref())?;
        self.rate_limiter.consume(
            u32::from(request.count).saturating_mul(2) + u32::from(request.position.is_some()),
        )?;
        if let Some(position) = &request.position {
            let (x, y) = self.resolve_position(position)?;
            self.fake_input(MOTION_NOTIFY, 0, x, y)?;
        } else {
            let pointer = self.pointer_position()?;
            self.ensure_point_allowed(pointer.x, pointer.y)?;
        }
        for index in 0..request.count {
            self.fake_input(BUTTON_PRESS, request.button, 0, 0)?;
            self.fake_input(BUTTON_RELEASE, request.button, 0, 0)?;
            if index + 1 < request.count {
                thread::sleep(Duration::from_millis(50));
            }
        }
        self.connection
            .flush()
            .map_err(|error| ControllerError::x11("flush click", error))?;
        self.finish_action(request.observe_after.as_ref(), &baseline, Vec::new())
    }

    fn drag(&mut self, request: DragRequest) -> Result<ActionResult> {
        self.ensure_xtest()?;
        self.validate_guard(&request.guard, true, false, &[&request.from, &request.to])?;
        if request.button == 0 || request.steps == 0 || request.steps > 100 {
            return Err(ControllerError::new(
                ErrorCode::InvalidInput,
                "button must be non-zero and drag steps must be between 1 and 100",
            ));
        }
        if request.duration_ms > 10_000 {
            return Err(ControllerError::new(
                ErrorCode::InvalidInput,
                "drag duration cannot exceed 10000 ms",
            ));
        }
        let baseline = self.action_baseline(request.observe_after.as_ref())?;
        self.rate_limiter.consume(u32::from(request.steps) + 3)?;
        let (from_x, from_y) = self.resolve_position(&request.from)?;
        let (to_x, to_y) = self.resolve_position(&request.to)?;
        self.fake_input(MOTION_NOTIFY, 0, from_x, from_y)?;
        self.fake_input(BUTTON_PRESS, request.button, 0, 0)?;
        self.held_buttons.insert(request.button);
        let sleep = Duration::from_millis(request.duration_ms / u64::from(request.steps));
        for step in 1..=request.steps {
            let fraction = f64::from(step) / f64::from(request.steps);
            let x = f64::from(from_x) + f64::from(to_x - from_x) * fraction;
            let y = f64::from(from_y) + f64::from(to_y - from_y) * fraction;
            self.fake_input(MOTION_NOTIFY, 0, x.round() as i32, y.round() as i32)?;
            if !sleep.is_zero() {
                thread::sleep(sleep);
            }
        }
        self.fake_input(BUTTON_RELEASE, request.button, 0, 0)?;
        self.held_buttons.remove(&request.button);
        self.connection
            .flush()
            .map_err(|error| ControllerError::x11("flush drag", error))?;
        self.finish_action(request.observe_after.as_ref(), &baseline, Vec::new())
    }

    fn scroll(&mut self, request: ScrollRequest) -> Result<ActionResult> {
        self.ensure_xtest()?;
        let positions = request
            .position
            .as_ref()
            .map_or_else(Vec::new, |position| vec![position]);
        self.validate_guard(
            &request.guard,
            request.position.is_some(),
            false,
            &positions,
        )?;
        let ticks = request
            .dx
            .unsigned_abs()
            .saturating_add(request.dy.unsigned_abs());
        if ticks == 0 || ticks > 100 {
            return Err(ControllerError::new(
                ErrorCode::InvalidInput,
                "scroll must contain between 1 and 100 total ticks",
            ));
        }
        let baseline = self.action_baseline(request.observe_after.as_ref())?;
        self.rate_limiter
            .consume(ticks.saturating_mul(2) + u32::from(request.position.is_some()))?;
        if let Some(position) = &request.position {
            let (x, y) = self.resolve_position(position)?;
            self.fake_input(MOTION_NOTIFY, 0, x, y)?;
        } else {
            let pointer = self.pointer_position()?;
            self.ensure_point_allowed(pointer.x, pointer.y)?;
        }
        self.scroll_axis(request.dy, 4, 5)?;
        self.scroll_axis(request.dx, 6, 7)?;
        self.connection
            .flush()
            .map_err(|error| ControllerError::x11("flush scroll", error))?;
        self.finish_action(request.observe_after.as_ref(), &baseline, Vec::new())
    }

    fn scroll_axis(&self, amount: i32, negative_button: u8, positive_button: u8) -> Result<()> {
        let button = if amount < 0 {
            negative_button
        } else {
            positive_button
        };
        for _ in 0..amount.unsigned_abs() {
            self.fake_input(BUTTON_PRESS, button, 0, 0)?;
            self.fake_input(BUTTON_RELEASE, button, 0, 0)?;
        }
        Ok(())
    }

    fn key(&mut self, request: KeyRequest) -> Result<ActionResult> {
        self.ensure_xtest()?;
        self.validate_guard(&request.guard, false, false, &[])?;
        self.ensure_focused_allowed()?;
        if request.keys.is_empty() || request.keys.len() > 16 {
            return Err(ControllerError::new(
                ErrorCode::InvalidInput,
                "keys must contain between 1 and 16 key names",
            ));
        }
        let baseline = self.action_baseline(request.observe_after.as_ref())?;
        let keyboard = KeyboardMap::load(&self.connection)?;
        let mut keycodes = Vec::new();
        for key in &request.keys {
            let stroke = keyboard.named_key(key)?;
            if stroke.mode_switch {
                push_unique(&mut keycodes, keyboard.mode_switch_keycode()?);
            }
            if stroke.shift {
                push_unique(&mut keycodes, keyboard.shift_keycode()?);
            }
            push_unique(&mut keycodes, stroke.keycode);
        }
        let event_count = match request.mode {
            KeyMode::Press => keycodes.len().saturating_mul(2),
            KeyMode::Down | KeyMode::Up => keycodes.len(),
        };
        self.rate_limiter
            .consume(u32::try_from(event_count).unwrap_or(u32::MAX))?;
        match request.mode {
            KeyMode::Press => {
                for keycode in &keycodes {
                    self.emit_key(*keycode, true)?;
                }
                for keycode in keycodes.iter().rev() {
                    self.emit_key(*keycode, false)?;
                }
            }
            KeyMode::Down => {
                for keycode in &keycodes {
                    if self.held_keys.insert(*keycode) {
                        self.emit_key(*keycode, true)?;
                    }
                }
            }
            KeyMode::Up => {
                for keycode in keycodes.iter().rev() {
                    self.emit_key(*keycode, false)?;
                    self.held_keys.remove(keycode);
                }
            }
        }
        self.connection
            .flush()
            .map_err(|error| ControllerError::x11("flush keyboard input", error))?;
        self.finish_action(request.observe_after.as_ref(), &baseline, Vec::new())
    }

    fn type_text(&mut self, request: TypeTextRequest) -> Result<ActionResult> {
        self.ensure_xtest()?;
        self.validate_guard(&request.guard, false, false, &[])?;
        self.ensure_focused_allowed()?;
        if request.text.len() > 1_048_576 {
            return Err(ControllerError::new(
                ErrorCode::InvalidInput,
                "text cannot exceed 1 MiB",
            ));
        }
        let baseline = self.action_baseline(request.observe_after.as_ref())?;
        let keyboard = KeyboardMap::load(&self.connection)?;
        let strokes = keyboard.text_strokes(&request.text);
        let use_clipboard = match request.method {
            TextMethod::Auto => strokes.is_none(),
            TextMethod::Keystrokes => false,
            TextMethod::Clipboard => true,
        };
        let mut warnings = Vec::new();
        if use_clipboard {
            let restored = self.type_via_clipboard(request.text.as_bytes(), &keyboard)?;
            if !restored {
                warnings.push("previous clipboard contents could not be restored".to_owned());
            }
        } else {
            let strokes = strokes.ok_or_else(|| {
                ControllerError::new(
                    ErrorCode::UnsupportedCapability,
                    "text contains characters unavailable in the active keyboard map",
                )
            })?;
            let count = strokes.iter().fold(0_u32, |total, stroke| {
                total + 2 + u32::from(stroke.shift) * 2 + u32::from(stroke.mode_switch) * 2
            });
            self.rate_limiter.consume(count)?;
            for stroke in strokes {
                self.type_stroke(&keyboard, stroke)?;
            }
            self.connection
                .flush()
                .map_err(|error| ControllerError::x11("flush text input", error))?;
        }
        self.finish_action(request.observe_after.as_ref(), &baseline, warnings)
    }

    fn type_stroke(&self, keyboard: &KeyboardMap, stroke: KeyStroke) -> Result<()> {
        let mode = stroke
            .mode_switch
            .then(|| keyboard.mode_switch_keycode())
            .transpose()?;
        let shift = stroke.shift.then(|| keyboard.shift_keycode()).transpose()?;
        if let Some(keycode) = mode {
            self.emit_key(keycode, true)?;
        }
        if let Some(keycode) = shift {
            self.emit_key(keycode, true)?;
        }
        self.emit_key(stroke.keycode, true)?;
        self.emit_key(stroke.keycode, false)?;
        if let Some(keycode) = shift {
            self.emit_key(keycode, false)?;
        }
        if let Some(keycode) = mode {
            self.emit_key(keycode, false)?;
        }
        Ok(())
    }

    fn window_action(&mut self, request: WindowActionRequest) -> Result<ActionResult> {
        self.validate_guard(&request.guard, false, false, &[])?;
        let baseline = self.action_baseline(request.observe_after.as_ref())?;
        let window = self.resolve_window(&request.window_ref)?;
        self.ensure_window_allowed(&window)?;
        let xid = self.xid_for_ref(&request.window_ref)?;
        match request.action {
            WindowAction::Move { x, y } => self.move_resize_window(xid, x, y, None, None)?,
            WindowAction::Resize { width, height } => {
                validate_window_size(width, height)?;
                self.move_resize_window(
                    xid,
                    window.geometry.x,
                    window.geometry.y,
                    Some(width),
                    Some(height),
                )?;
            }
            WindowAction::MoveResize {
                x,
                y,
                width,
                height,
            } => {
                validate_window_size(width, height)?;
                self.move_resize_window(xid, x, y, Some(width), Some(height))?;
            }
            WindowAction::Minimize => {
                self.send_root_message(xid, self.atoms.WM_CHANGE_STATE, [3, 0, 0, 0, 0])?;
            }
            WindowAction::Maximize => self.change_maximized_state(xid, 1)?,
            WindowAction::Restore => self.change_maximized_state(xid, 0)?,
            WindowAction::Close { force } => self.close_window(xid, force)?,
        }
        self.connection
            .flush()
            .map_err(|error| ControllerError::x11("flush window action", error))?;
        self.finish_action(request.observe_after.as_ref(), &baseline, Vec::new())
    }

    fn move_resize_window(
        &self,
        xid: Window,
        x: i32,
        y: i32,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Result<()> {
        if self.capabilities.ewmh {
            let mut flags = 2_u32 << 12;
            flags |= 1 << 8;
            flags |= 1 << 9;
            if width.is_some() {
                flags |= 1 << 10;
            }
            if height.is_some() {
                flags |= 1 << 11;
            }
            self.send_root_message(
                xid,
                self.atoms._NET_MOVERESIZE_WINDOW,
                [
                    flags,
                    x.cast_unsigned(),
                    y.cast_unsigned(),
                    width.unwrap_or(0),
                    height.unwrap_or(0),
                ],
            )
        } else {
            let mut aux = ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .stack_mode(StackMode::ABOVE);
            if let Some(width) = width {
                aux = aux.width(width);
            }
            if let Some(height) = height {
                aux = aux.height(height);
            }
            self.connection
                .configure_window(xid, &aux)
                .map_err(|error| ControllerError::x11("configure window", error))?
                .check()
                .map_err(|error| ControllerError::x11("configure window", error))
        }
    }

    fn change_maximized_state(&self, xid: Window, action: u32) -> Result<()> {
        if !self.capabilities.ewmh {
            return Err(ControllerError::new(
                ErrorCode::UnsupportedCapability,
                "maximize and restore require an EWMH window manager",
            ));
        }
        self.send_root_message(
            xid,
            self.atoms._NET_WM_STATE,
            [
                action,
                self.atoms._NET_WM_STATE_MAXIMIZED_VERT,
                self.atoms._NET_WM_STATE_MAXIMIZED_HORZ,
                2,
                0,
            ],
        )
    }

    fn close_window(&self, xid: Window, force: bool) -> Result<()> {
        let protocols =
            property_u32_list(&self.connection, xid, self.atoms.WM_PROTOCOLS)?.unwrap_or_default();
        if protocols.contains(&self.atoms.WM_DELETE_WINDOW) {
            let event = ClientMessageEvent::new(
                32,
                xid,
                self.atoms.WM_PROTOCOLS,
                ClientMessageData::from([self.atoms.WM_DELETE_WINDOW, CURRENT_TIME, 0, 0, 0]),
            );
            self.connection
                .send_event(false, xid, EventMask::NO_EVENT, event)
                .map_err(|error| ControllerError::x11("request window close", error))?
                .check()
                .map_err(|error| ControllerError::x11("request window close", error))
        } else if force {
            self.connection
                .kill_client(xid)
                .map_err(|error| ControllerError::x11("kill X11 client", error))?
                .check()
                .map_err(|error| ControllerError::x11("kill X11 client", error))
        } else {
            Err(ControllerError::new(
                ErrorCode::UnsupportedCapability,
                "window does not support WM_DELETE_WINDOW; retry with force=true to kill its X11 client",
            ))
        }
    }

    fn send_root_message(&self, xid: Window, type_: Atom, data: [u32; 5]) -> Result<()> {
        let event = ClientMessageEvent::new(32, xid, type_, ClientMessageData::from(data));
        self.connection
            .send_event(
                false,
                self.root,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                event,
            )
            .map_err(|error| ControllerError::x11("send window-manager message", error))?
            .check()
            .map_err(|error| ControllerError::x11("send window-manager message", error))
    }

    fn xid_for_ref(&self, window_ref: &str) -> Result<Window> {
        self.ref_xids.get(window_ref).copied().ok_or_else(|| {
            ControllerError::new(
                ErrorCode::StaleWindow,
                format!("unknown or stale window reference: {window_ref}"),
            )
        })
    }

    fn resolve_position(&mut self, position: &Position) -> Result<(i32, i32)> {
        let (x, y, target) = match position {
            Position::Screen { x, y } => (*x, *y, None),
            Position::Window { window_ref, x, y } => {
                let window = self.resolve_window(window_ref)?;
                self.ensure_window_allowed(&window)?;
                if *x < 0
                    || *y < 0
                    || i64::from(*x) >= i64::from(window.geometry.width)
                    || i64::from(*y) >= i64::from(window.geometry.height)
                {
                    return Err(ControllerError::new(
                        ErrorCode::InvalidInput,
                        "window coordinates are outside the referenced window",
                    ));
                }
                (
                    window.geometry.x.saturating_add(*x),
                    window.geometry.y.saturating_add(*y),
                    Some(window),
                )
            }
            Position::WindowRelative { window_ref, x, y } => {
                if !x.is_finite()
                    || !y.is_finite()
                    || !(0.0..=1.0).contains(x)
                    || !(0.0..=1.0).contains(y)
                {
                    return Err(ControllerError::new(
                        ErrorCode::InvalidInput,
                        "window-relative coordinates must be finite values between 0 and 1",
                    ));
                }
                let window = self.resolve_window(window_ref)?;
                self.ensure_window_allowed(&window)?;
                let local_x =
                    (x * f64::from(window.geometry.width.saturating_sub(1))).round() as i32;
                let local_y =
                    (y * f64::from(window.geometry.height.saturating_sub(1))).round() as i32;
                (
                    window.geometry.x.saturating_add(local_x),
                    window.geometry.y.saturating_add(local_y),
                    Some(window),
                )
            }
        };
        let width = i32::from(self.capabilities.screen.width);
        let height = i32::from(self.capabilities.screen.height);
        if x < 0 || y < 0 || x >= width || y >= height {
            return Err(ControllerError::new(
                ErrorCode::InvalidInput,
                format!("screen coordinate ({x}, {y}) is outside {width}x{height}"),
            ));
        }
        if target.is_none() {
            self.ensure_point_allowed(x, y)?;
        }
        Ok((x, y))
    }

    fn ensure_point_allowed(&mut self, x: i32, y: i32) -> Result<()> {
        if self.allowlist.is_empty() {
            return Ok(());
        }
        let list = self.window_list()?;
        let window = list.windows.iter().rev().find(|window| {
            let geometry = window.geometry;
            x >= geometry.x
                && y >= geometry.y
                && i64::from(x) < i64::from(geometry.x) + i64::from(geometry.width)
                && i64::from(y) < i64::from(geometry.y) + i64::from(geometry.height)
        });
        let Some(window) = window else {
            return Err(ControllerError::new(
                ErrorCode::AccessDenied,
                "coordinate does not resolve to an allowlisted window",
            ));
        };
        self.ensure_window_allowed(window)
    }

    fn ensure_window_allowed(&self, window: &WindowInfo) -> Result<()> {
        if self.allowlist.is_empty() {
            return Ok(());
        }
        let allowed = window
            .class
            .instance
            .iter()
            .chain(window.class.name.iter())
            .any(|class| self.allowlist.iter().any(|matcher| matcher.is_match(class)));
        if allowed {
            Ok(())
        } else {
            Err(ControllerError::new(
                ErrorCode::AccessDenied,
                format!(
                    "window {} is not permitted by --allow-window-class",
                    window.window_ref
                ),
            ))
        }
    }

    fn ensure_focused_allowed(&mut self) -> Result<()> {
        if self.allowlist.is_empty() {
            return Ok(());
        }
        let list = self.window_list()?;
        let active = list.active_window.ok_or_else(|| {
            ControllerError::new(
                ErrorCode::AccessDenied,
                "keyboard input is blocked because no allowlisted window is focused",
            )
        })?;
        let window = list
            .windows
            .iter()
            .find(|window| window.window_ref == active)
            .ok_or_else(|| {
                ControllerError::new(
                    ErrorCode::AccessDenied,
                    "keyboard input is blocked because the focused window is unknown",
                )
            })?;
        self.ensure_window_allowed(window)
    }

    fn fake_input(&self, event_type: u8, detail: u8, x: i32, y: i32) -> Result<()> {
        let x = i16::try_from(x).map_err(|_| {
            ControllerError::new(
                ErrorCode::InvalidInput,
                "pointer x coordinate is out of range",
            )
        })?;
        let y = i16::try_from(y).map_err(|_| {
            ControllerError::new(
                ErrorCode::InvalidInput,
                "pointer y coordinate is out of range",
            )
        })?;
        self.connection
            .xtest_fake_input(event_type, detail, CURRENT_TIME, self.root, x, y, 0)
            .map_err(|error| ControllerError::x11("synthesize input", error))?
            .check()
            .map_err(|error| ControllerError::x11("synthesize input", error))
    }

    fn emit_key(&self, keycode: u8, pressed: bool) -> Result<()> {
        self.fake_input(if pressed { KEY_PRESS } else { KEY_RELEASE }, keycode, 0, 0)
    }

    fn release_held_input(&mut self) {
        for keycode in self.held_keys.drain() {
            let _ = self.connection.xtest_fake_input(
                KEY_RELEASE,
                keycode,
                CURRENT_TIME,
                self.root,
                0,
                0,
                0,
            );
        }
        for button in self.held_buttons.drain() {
            let _ = self.connection.xtest_fake_input(
                BUTTON_RELEASE,
                button,
                CURRENT_TIME,
                self.root,
                0,
                0,
                0,
            );
        }
        let _ = self.connection.flush();
    }
}

fn validate_window_size(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 || width > u32::from(u16::MAX) || height > u32::from(u16::MAX) {
        Err(ControllerError::new(
            ErrorCode::InvalidInput,
            "window width and height must be between 1 and 65535",
        ))
    } else {
        Ok(())
    }
}

fn push_unique(values: &mut Vec<u8>, value: u8) {
    if !values.contains(&value) {
        values.push(value);
    }
}

impl Actor {
    fn wait_for(&mut self, request: WaitRequest) -> Result<WaitResult> {
        if request.timeout_ms == 0 || request.timeout_ms > 60_000 {
            return Err(ControllerError::new(
                ErrorCode::InvalidInput,
                "timeout_ms must be between 1 and 60000",
            ));
        }
        let deadline = Instant::now() + Duration::from_millis(request.timeout_ms);
        match request.condition {
            WaitCondition::Change {
                since_frame_id,
                target,
            } => {
                let observe_request = ObserveRequest {
                    target: target.clone(),
                    include_windows: true,
                    delivery: ObservationDelivery::Full,
                };
                if self.damage.is_some() {
                    self.process_pending_events()?;
                    let bounds = self.resolve_observe_target(&target)?;
                    let baseline_sequence = if let Some(frame_id) = since_frame_id {
                        let frame = self
                            .frame_history
                            .iter()
                            .find(|frame| frame.frame_id == frame_id)
                            .ok_or_else(|| {
                                ControllerError::new(
                                    ErrorCode::StaleFrame,
                                    format!("frame {frame_id} is not in the 64-frame history"),
                                )
                                .retryable(true)
                            })?;
                        if frame.target != target {
                            return Err(ControllerError::new(
                                ErrorCode::StaleFrame,
                                "frame_changed target does not match the base frame target",
                            )
                            .retryable(true));
                        }
                        frame.damage_sequence
                    } else {
                        self.damage_sequence
                    };
                    loop {
                        self.process_pending_events()?;
                        if self.damage_history.front().is_some_and(|record| {
                            record.sequence > baseline_sequence.saturating_add(1)
                                && self.damage_sequence > baseline_sequence
                        }) {
                            return Err(ControllerError::new(
                                ErrorCode::StaleFrame,
                                "damage history for the waited frame has expired",
                            )
                            .retryable(true));
                        }
                        let changed = self
                            .damage_history
                            .iter()
                            .filter(|record| record.sequence > baseline_sequence)
                            .any(|record| {
                                record.topology
                                    || intersect_geometry(record.bounds, bounds).is_some()
                            });
                        if changed {
                            let observation = request
                                .observe
                                .then(|| self.observe(observe_request.clone()))
                                .transpose()?;
                            return Ok(WaitResult {
                                matched: true,
                                window: None,
                                observation,
                            });
                        }
                        self.wait_tick(deadline)?;
                    }
                }
                let baseline_signature = if let Some(frame_id) = since_frame_id {
                    let frame = self
                        .frame_history
                        .iter()
                        .find(|frame| frame.frame_id == frame_id)
                        .ok_or_else(|| {
                            ControllerError::new(
                                ErrorCode::StaleFrame,
                                format!("frame {frame_id} is not in the 64-frame history"),
                            )
                            .retryable(true)
                        })?;
                    if frame.target != target {
                        return Err(ControllerError::new(
                            ErrorCode::StaleFrame,
                            "frame_changed target does not match the base frame target",
                        )
                        .retryable(true));
                    }
                    frame.signature
                } else {
                    self.observe(observe_request.clone())?.signature
                };
                loop {
                    let observation = self.observe(observe_request.clone())?;
                    if observation.signature != baseline_signature {
                        return Ok(WaitResult {
                            matched: true,
                            window: None,
                            observation: request.observe.then_some(observation),
                        });
                    }
                    self.poll_tick(deadline)?;
                }
            }
            WaitCondition::Idle { quiet_ms, target } => {
                if quiet_ms > request.timeout_ms {
                    return Err(ControllerError::new(
                        ErrorCode::InvalidInput,
                        "frame_idle quiet_ms cannot exceed timeout_ms",
                    ));
                }
                let quiet = Duration::from_millis(quiet_ms);
                let observe_request = ObserveRequest {
                    target: target.clone(),
                    include_windows: true,
                    delivery: ObservationDelivery::Full,
                };
                if self.damage.is_some() {
                    self.process_pending_events()?;
                    let mut bounds = self.resolve_observe_target(&target)?;
                    let mut sequence = self.damage_sequence;
                    let mut quiet_since = Instant::now();
                    loop {
                        self.process_pending_events()?;
                        let relevant = self
                            .damage_history
                            .iter()
                            .filter(|record| record.sequence > sequence)
                            .any(|record| {
                                record.topology
                                    || intersect_geometry(record.bounds, bounds).is_some()
                            });
                        if relevant {
                            sequence = self.damage_sequence;
                            bounds = self.resolve_observe_target(&target)?;
                            quiet_since = Instant::now();
                        } else {
                            sequence = self.damage_sequence;
                        }
                        if quiet_since.elapsed() >= quiet {
                            let observation = request
                                .observe
                                .then(|| self.observe(observe_request.clone()))
                                .transpose()?;
                            return Ok(WaitResult {
                                matched: true,
                                window: None,
                                observation,
                            });
                        }
                        if Instant::now() >= deadline {
                            return Err(ControllerError::new(
                                ErrorCode::Timeout,
                                "wait condition timed out",
                            )
                            .retryable(true));
                        }
                        self.wait_for_x11_event_until((quiet_since + quiet).min(deadline))?;
                    }
                }
                let mut observation = self.observe(observe_request.clone())?;
                let mut signature = observation.signature;
                let mut quiet_since = Instant::now();
                loop {
                    if quiet_since.elapsed() >= quiet {
                        return Ok(WaitResult {
                            matched: true,
                            window: None,
                            observation: request.observe.then_some(observation),
                        });
                    }
                    self.poll_tick(deadline)?;
                    let next = self.observe(observe_request.clone())?;
                    if next.signature != signature {
                        quiet_since = Instant::now();
                        signature = next.signature;
                    }
                    observation = next;
                }
            }
            WaitCondition::Window { selector } => loop {
                let list = self.window_list()?;
                if let Some(window) = list
                    .windows
                    .into_iter()
                    .find(|window| selector.matches(window))
                {
                    let observation = request
                        .observe
                        .then(|| self.observe(ObserveRequest::default()))
                        .transpose()?;
                    return Ok(WaitResult {
                        matched: true,
                        window: Some(window),
                        observation,
                    });
                }
                self.wait_tick(deadline)?;
            },
            WaitCondition::WindowState {
                window_ref,
                mapped,
                active,
                title_contains,
            } => loop {
                let list = self.window_list()?;
                let is_active = list.active_window.as_deref() == Some(window_ref.as_str());
                if let Some(window) = list
                    .windows
                    .into_iter()
                    .find(|window| window.window_ref == window_ref)
                {
                    let matches = mapped.is_none_or(|wanted| window.mapped == wanted)
                        && active.is_none_or(|wanted| is_active == wanted)
                        && title_contains.as_ref().is_none_or(|wanted| {
                            window.title.to_lowercase().contains(&wanted.to_lowercase())
                        });
                    if matches {
                        let observation = request
                            .observe
                            .then(|| self.observe(ObserveRequest::default()))
                            .transpose()?;
                        return Ok(WaitResult {
                            matched: true,
                            window: Some(window),
                            observation,
                        });
                    }
                }
                self.wait_tick(deadline)?;
            },
            WaitCondition::Focus { window_ref } => loop {
                let list = self.window_list()?;
                if list.active_window.as_deref() == Some(window_ref.as_str()) {
                    let window = list
                        .windows
                        .into_iter()
                        .find(|window| window.window_ref == window_ref);
                    let observation = request
                        .observe
                        .then(|| self.observe(ObserveRequest::default()))
                        .transpose()?;
                    return Ok(WaitResult {
                        matched: true,
                        window,
                        observation,
                    });
                }
                self.wait_tick(deadline)?;
            },
            WaitCondition::WindowClosed { window_ref } => {
                let xid = self.xid_for_ref(&window_ref)?;
                loop {
                    let closed = self.window_info(xid).is_err()
                        || self
                            .window_refs
                            .get(&xid)
                            .is_none_or(|current| current != &window_ref);
                    if closed {
                        let observation = request
                            .observe
                            .then(|| self.observe(ObserveRequest::default()))
                            .transpose()?;
                        return Ok(WaitResult {
                            matched: true,
                            window: None,
                            observation,
                        });
                    }
                    self.wait_tick(deadline)?;
                }
            }
        }
    }

    fn wait_tick(&mut self, deadline: Instant) -> Result<()> {
        if Instant::now() >= deadline {
            return Err(
                ControllerError::new(ErrorCode::Timeout, "wait condition timed out")
                    .retryable(true),
            );
        }
        self.wait_for_x11_event_until(deadline)
    }

    fn wait_for_x11_event_until(&mut self, deadline: Instant) -> Result<()> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            let timeout = Timespec::try_from(remaining).map_err(|error| {
                ControllerError::new(
                    ErrorCode::Internal,
                    format!("convert X11 wait deadline: {error}"),
                )
            })?;
            {
                let mut descriptors = [PollFd::new(self.connection.stream(), PollFlags::IN)];
                poll(&mut descriptors, Some(&timeout))
                    .map_err(|error| ControllerError::x11("wait for X11 event", error))?;
            }
        }
        self.process_pending_events()
    }

    fn poll_tick(&mut self, deadline: Instant) -> Result<()> {
        let now = Instant::now();
        if now >= deadline {
            return Err(
                ControllerError::new(ErrorCode::Timeout, "wait condition timed out")
                    .retryable(true),
            );
        }
        thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
        self.process_pending_events()
    }

    fn type_via_clipboard(&mut self, text: &[u8], keyboard: &KeyboardMap) -> Result<bool> {
        let (previous, previous_read) = self.read_clipboard();
        self.clipboard_content = Some(text.to_vec());
        self.connection
            .set_selection_owner(self.clipboard_window, self.atoms.CLIPBOARD, CURRENT_TIME)
            .map_err(|error| ControllerError::x11("own CLIPBOARD selection", error))?
            .check()
            .map_err(|error| ControllerError::x11("own CLIPBOARD selection", error))?;
        self.connection
            .flush()
            .map_err(|error| ControllerError::x11("flush clipboard ownership", error))?;
        let owner = self
            .connection
            .get_selection_owner(self.atoms.CLIPBOARD)
            .map_err(|error| ControllerError::x11("request clipboard owner", error))?
            .reply()
            .map_err(|error| ControllerError::x11("read clipboard owner", error))?
            .owner;
        if owner != self.clipboard_window {
            return Err(ControllerError::new(
                ErrorCode::X11,
                "failed to acquire CLIPBOARD selection",
            ));
        }

        let mut keycodes = Vec::new();
        for key in &self.config.paste_chord {
            let stroke = keyboard.named_key(key)?;
            if stroke.mode_switch {
                push_unique(&mut keycodes, keyboard.mode_switch_keycode()?);
            }
            if stroke.shift {
                push_unique(&mut keycodes, keyboard.shift_keycode()?);
            }
            push_unique(&mut keycodes, stroke.keycode);
        }
        self.rate_limiter
            .consume(u32::try_from(keycodes.len().saturating_mul(2)).unwrap_or(u32::MAX))?;
        for keycode in &keycodes {
            self.emit_key(*keycode, true)?;
        }
        for keycode in keycodes.iter().rev() {
            self.emit_key(*keycode, false)?;
        }
        self.connection
            .flush()
            .map_err(|error| ControllerError::x11("flush paste chord", error))?;
        let _paste_requested = self.wait_for_selection_request(CLIPBOARD_TIMEOUT)?;

        if let Some(previous) = previous {
            self.clipboard_content = Some(previous);
        } else {
            self.clipboard_content = None;
            self.connection
                .set_selection_owner(NONE, self.atoms.CLIPBOARD, CURRENT_TIME)
                .map_err(|error| ControllerError::x11("release CLIPBOARD selection", error))?
                .check()
                .map_err(|error| ControllerError::x11("release CLIPBOARD selection", error))?;
        }
        self.connection
            .flush()
            .map_err(|error| ControllerError::x11("restore clipboard", error))?;
        Ok(previous_read)
    }

    fn read_clipboard(&mut self) -> (Option<Vec<u8>>, bool) {
        let result = (|| -> Result<Option<Vec<u8>>> {
            let owner = self
                .connection
                .get_selection_owner(self.atoms.CLIPBOARD)
                .map_err(|error| ControllerError::x11("request clipboard owner", error))?
                .reply()
                .map_err(|error| ControllerError::x11("read clipboard owner", error))?
                .owner;
            if owner == NONE {
                return Ok(None);
            }
            if owner == self.clipboard_window {
                return Ok(self.clipboard_content.clone());
            }
            self.connection
                .delete_property(self.clipboard_window, self.atoms.X11_MCP_SELECTION)
                .map_err(|error| {
                    ControllerError::x11("clear clipboard transfer property", error)
                })?;
            self.connection
                .convert_selection(
                    self.clipboard_window,
                    self.atoms.CLIPBOARD,
                    self.atoms.UTF8_STRING,
                    self.atoms.X11_MCP_SELECTION,
                    CURRENT_TIME,
                )
                .map_err(|error| ControllerError::x11("request clipboard contents", error))?;
            self.connection
                .flush()
                .map_err(|error| ControllerError::x11("flush clipboard request", error))?;
            let deadline = Instant::now() + CLIPBOARD_TIMEOUT;
            loop {
                while let Some(event) = self
                    .connection
                    .poll_for_event()
                    .map_err(|error| ControllerError::x11("poll clipboard event", error))?
                {
                    match event {
                        Event::SelectionNotify(event)
                            if event.requestor == self.clipboard_window
                                && event.selection == self.atoms.CLIPBOARD =>
                        {
                            if event.property == NONE {
                                return Err(ControllerError::new(
                                    ErrorCode::UnsupportedCapability,
                                    "clipboard owner does not offer UTF8_STRING",
                                ));
                            }
                            let reply = self
                                .connection
                                .get_property(
                                    true,
                                    self.clipboard_window,
                                    event.property,
                                    AtomEnum::ANY,
                                    0,
                                    262_144,
                                )
                                .map_err(|error| {
                                    ControllerError::x11("request clipboard property", error)
                                })?
                                .reply()
                                .map_err(|error| {
                                    ControllerError::x11("read clipboard property", error)
                                })?;
                            if reply.bytes_after != 0 {
                                return Err(ControllerError::new(
                                    ErrorCode::UnsupportedCapability,
                                    "clipboard contents exceed the 1 MiB preservation limit",
                                ));
                            }
                            return Ok(Some(reply.value));
                        }
                        Event::SelectionRequest(event) => self.serve_selection_request(event)?,
                        Event::SelectionClear(event) if event.selection == self.atoms.CLIPBOARD => {
                            self.clipboard_content = None;
                        }
                        _ => {}
                    }
                }
                if Instant::now() >= deadline {
                    return Err(ControllerError::new(
                        ErrorCode::Timeout,
                        "timed out reading prior clipboard contents",
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
        })();
        match result {
            Ok(value) => (value, true),
            Err(error) => {
                debug!(%error, "could not preserve prior clipboard contents");
                (None, false)
            }
        }
    }

    fn wait_for_selection_request(&mut self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            while let Some(event) = self
                .connection
                .poll_for_event()
                .map_err(|error| ControllerError::x11("poll paste event", error))?
            {
                match event {
                    Event::SelectionRequest(event) => {
                        let is_clipboard = event.selection == self.atoms.CLIPBOARD;
                        self.serve_selection_request(event)?;
                        if is_clipboard {
                            return Ok(true);
                        }
                    }
                    Event::SelectionClear(event) if event.selection == self.atoms.CLIPBOARD => {
                        self.clipboard_content = None;
                        return Ok(false);
                    }
                    Event::DamageNotify(event) if Some(event.damage) == self.damage => {
                        self.record_damage_event(event)?;
                    }
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn process_pending_events(&mut self) -> Result<()> {
        while let Some(event) = self
            .connection
            .poll_for_event()
            .map_err(|error| ControllerError::x11("poll X11 event", error))?
        {
            match event {
                Event::SelectionRequest(event) => self.serve_selection_request(event)?,
                Event::SelectionClear(event) if event.selection == self.atoms.CLIPBOARD => {
                    self.clipboard_content = None;
                }
                Event::DamageNotify(event) if Some(event.damage) == self.damage => {
                    self.record_damage_event(event)?;
                }
                Event::DestroyNotify(event) => {
                    self.window_refs.remove(&event.window);
                    self.observed_windows.remove(&event.window);
                    self.record_topology_change();
                }
                Event::CreateNotify(_)
                | Event::MapNotify(_)
                | Event::UnmapNotify(_)
                | Event::ConfigureNotify(_)
                | Event::ReparentNotify(_)
                | Event::RandrNotify(_)
                | Event::RandrScreenChangeNotify(_)
                | Event::PropertyNotify(_) => self.record_topology_change(),
                Event::MappingNotify(_) => {
                    debug!("X11 keyboard mapping changed");
                    self.record_topology_change();
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn record_damage_event(&mut self, event: x11rb::protocol::damage::NotifyEvent) -> Result<()> {
        self.record_damage(
            Geometry {
                x: i32::from(event.area.x),
                y: i32::from(event.area.y),
                width: u32::from(event.area.width),
                height: u32::from(event.area.height),
            },
            false,
        );
        if let Some(damage) = self.damage {
            self.connection
                .damage_subtract(damage, NONE, NONE)
                .map_err(|error| ControllerError::x11("subtract Damage region", error))?
                .check()
                .map_err(|error| ControllerError::x11("subtract Damage region", error))?;
        }
        Ok(())
    }

    fn record_topology_change(&mut self) {
        self.record_damage(
            Geometry {
                x: 0,
                y: 0,
                width: u32::from(self.capabilities.screen.width),
                height: u32::from(self.capabilities.screen.height),
            },
            true,
        );
    }

    fn record_damage(&mut self, bounds: Geometry, topology: bool) {
        if bounds.width == 0 || bounds.height == 0 {
            return;
        }
        self.damage_sequence = self.damage_sequence.saturating_add(1);
        self.damage_history.push_back(DamageRecord {
            sequence: self.damage_sequence,
            bounds,
            topology,
        });
        while self.damage_history.len() > DAMAGE_HISTORY_SIZE {
            self.damage_history.pop_front();
        }
    }

    fn serve_selection_request(&self, request: SelectionRequestEvent) -> Result<()> {
        let property = if request.property == NONE {
            request.target
        } else {
            request.property
        };
        let accepted_property =
            if request.selection != self.atoms.CLIPBOARD || self.clipboard_content.is_none() {
                NONE
            } else if request.target == self.atoms.TARGETS {
                self.connection
                    .change_property32(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        AtomEnum::ATOM,
                        &[
                            self.atoms.TARGETS,
                            self.atoms.UTF8_STRING,
                            self.atoms.TEXT,
                            u32::from(AtomEnum::STRING),
                        ],
                    )
                    .map_err(|error| ControllerError::x11("serve clipboard targets", error))?
                    .check()
                    .map_err(|error| ControllerError::x11("serve clipboard targets", error))?;
                property
            } else if request.target == self.atoms.UTF8_STRING
                || request.target == self.atoms.TEXT
                || request.target == u32::from(AtomEnum::STRING)
            {
                let content = self.clipboard_content.as_deref().unwrap_or_default();
                self.connection
                    .change_property8(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        request.target,
                        content,
                    )
                    .map_err(|error| ControllerError::x11("serve clipboard text", error))?
                    .check()
                    .map_err(|error| ControllerError::x11("serve clipboard text", error))?;
                property
            } else {
                NONE
            };
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: request.sequence,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property: accepted_property,
        };
        self.connection
            .send_event(false, request.requestor, EventMask::NO_EVENT, notify)
            .map_err(|error| ControllerError::x11("notify clipboard requestor", error))?
            .check()
            .map_err(|error| ControllerError::x11("notify clipboard requestor", error))?;
        self.connection
            .flush()
            .map_err(|error| ControllerError::x11("flush clipboard response", error))
    }
}

fn probe_extensions(
    connection: &RustConnection,
) -> Result<std::collections::BTreeMap<String, bool>> {
    let names = [
        ("xtest", "XTEST"),
        ("randr", "RANDR"),
        ("xfixes", "XFIXES"),
        ("damage", "DAMAGE"),
        ("shm", "MIT-SHM"),
        ("composite", "Composite"),
        ("xkb", "XKEYBOARD"),
    ];
    names
        .into_iter()
        .map(|(key, extension)| {
            let present = connection
                .query_extension(extension.as_bytes())
                .map_err(|error| ControllerError::x11("query extension", error))?
                .reply()
                .map_err(|error| ControllerError::x11("query extension", error))?
                .present;
            Ok((key.to_owned(), present))
        })
        .collect()
}

fn query_monitors(
    connection: &RustConnection,
    root: Window,
    screen: &x11rb::protocol::xproto::Screen,
    extensions: &std::collections::BTreeMap<String, bool>,
) -> Vec<MonitorInfo> {
    if extensions.get("randr") == Some(&true)
        && let Ok(cookie) = connection.randr_get_monitors(root, true)
        && let Ok(reply) = cookie.reply()
    {
        let mut monitors = Vec::new();
        for monitor in reply.monitors {
            let name = connection
                .get_atom_name(monitor.name)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map_or_else(
                    || format!("monitor-{}", monitors.len() + 1),
                    |reply| String::from_utf8_lossy(&reply.name).into_owned(),
                );
            monitors.push(MonitorInfo {
                name,
                primary: monitor.primary,
                geometry: Geometry {
                    x: i32::from(monitor.x),
                    y: i32::from(monitor.y),
                    width: u32::from(monitor.width),
                    height: u32::from(monitor.height),
                },
            });
        }
        if !monitors.is_empty() {
            return monitors;
        }
    }
    vec![MonitorInfo {
        name: "screen".to_owned(),
        primary: true,
        geometry: Geometry {
            x: 0,
            y: 0,
            width: u32::from(screen.width_in_pixels),
            height: u32::from(screen.height_in_pixels),
        },
    }]
}

fn property_bytes(
    connection: &RustConnection,
    window: Window,
    property: Atom,
) -> Result<Option<Vec<u8>>> {
    let reply = connection
        .get_property(false, window, property, AtomEnum::ANY, 0, u32::MAX)
        .map_err(|error| ControllerError::x11("request window property", error))?
        .reply()
        .map_err(|error| ControllerError::x11("read window property", error))?;
    if reply.type_ == NONE {
        Ok(None)
    } else {
        Ok(Some(reply.value))
    }
}

fn property_u32(
    connection: &RustConnection,
    window: Window,
    property: Atom,
) -> Result<Option<u32>> {
    Ok(property_u32_list(connection, window, property)?
        .and_then(|values| values.into_iter().next()))
}

fn property_u32_list(
    connection: &RustConnection,
    window: Window,
    property: Atom,
) -> Result<Option<Vec<u32>>> {
    let reply = connection
        .get_property(false, window, property, AtomEnum::ANY, 0, u32::MAX)
        .map_err(|error| ControllerError::x11("request window property", error))?
        .reply()
        .map_err(|error| ControllerError::x11("read window property", error))?;
    Ok(reply.value32().map(Iterator::collect))
}

fn geometry_area(geometry: &Geometry) -> u64 {
    u64::from(geometry.width).saturating_mul(u64::from(geometry.height))
}

fn intersect_geometry(left: Geometry, right: Geometry) -> Option<Geometry> {
    let x1 = i64::from(left.x).max(i64::from(right.x));
    let y1 = i64::from(left.y).max(i64::from(right.y));
    let x2 = (i64::from(left.x) + i64::from(left.width))
        .min(i64::from(right.x) + i64::from(right.width));
    let y2 = (i64::from(left.y) + i64::from(left.height))
        .min(i64::from(right.y) + i64::from(right.height));
    (x2 > x1 && y2 > y1).then(|| Geometry {
        x: i32::try_from(x1).unwrap_or(i32::MAX),
        y: i32::try_from(y1).unwrap_or(i32::MAX),
        width: u32::try_from(x2 - x1).unwrap_or(u32::MAX),
        height: u32::try_from(y2 - y1).unwrap_or(u32::MAX),
    })
}

fn rectangles_touch(left: Geometry, right: Geometry) -> bool {
    let left_edge = i64::from(left.x) + i64::from(left.width);
    let left_bottom = i64::from(left.y) + i64::from(left.height);
    let right_edge = i64::from(right.x) + i64::from(right.width);
    let right_bottom = i64::from(right.y) + i64::from(right.height);
    i64::from(left.x) <= right_edge
        && i64::from(right.x) <= left_edge
        && i64::from(left.y) <= right_bottom
        && i64::from(right.y) <= left_bottom
}

fn union_geometry(left: Geometry, right: Geometry) -> Geometry {
    let x1 = i64::from(left.x).min(i64::from(right.x));
    let y1 = i64::from(left.y).min(i64::from(right.y));
    let x2 = (i64::from(left.x) + i64::from(left.width))
        .max(i64::from(right.x) + i64::from(right.width));
    let y2 = (i64::from(left.y) + i64::from(left.height))
        .max(i64::from(right.y) + i64::from(right.height));
    Geometry {
        x: i32::try_from(x1).unwrap_or(i32::MIN),
        y: i32::try_from(y1).unwrap_or(i32::MIN),
        width: u32::try_from(x2 - x1).unwrap_or(u32::MAX),
        height: u32::try_from(y2 - y1).unwrap_or(u32::MAX),
    }
}

fn delta_should_use_full(rectangles: &[Geometry], bounds: Geometry) -> bool {
    let changed_area = rectangles.iter().map(geometry_area).sum::<u64>();
    let full_area = geometry_area(&bounds);
    rectangles.len() > MAX_DELTA_PATCHES
        || changed_area.saturating_mul(100) >= full_area.saturating_mul(FULL_FRAME_AREA_PERCENT)
}

fn coalesce_rectangles(rectangles: &mut Vec<Geometry>) {
    let mut index = 0;
    while index < rectangles.len() {
        let mut candidate = index + 1;
        while candidate < rectangles.len() {
            if rectangles_touch(rectangles[index], rectangles[candidate]) {
                rectangles[index] = union_geometry(rectangles[index], rectangles[candidate]);
                rectangles.swap_remove(candidate);
                candidate = index + 1;
            } else {
                candidate += 1;
            }
        }
        index += 1;
    }
    rectangles.sort_by_key(|geometry| (geometry.y, geometry.x));
}

fn clip_geometry(geometry: Geometry, screen_width: u32, screen_height: u32) -> Result<Geometry> {
    if geometry.width == 0 || geometry.height == 0 {
        return Err(ControllerError::new(
            ErrorCode::InvalidInput,
            "capture dimensions must be non-zero",
        ));
    }
    let x = geometry.x.max(0);
    let y = geometry.y.max(0);
    let right = i64::from(geometry.x) + i64::from(geometry.width);
    let bottom = i64::from(geometry.y) + i64::from(geometry.height);
    let clipped_right = right.min(i64::from(screen_width));
    let clipped_bottom = bottom.min(i64::from(screen_height));
    if clipped_right <= i64::from(x) || clipped_bottom <= i64::from(y) {
        return Err(ControllerError::new(
            ErrorCode::InvalidInput,
            "capture region is outside the screen",
        ));
    }
    Ok(Geometry {
        x,
        y,
        width: u32::try_from(clipped_right - i64::from(x)).unwrap_or(0),
        height: u32::try_from(clipped_bottom - i64::from(y)).unwrap_or(0),
    })
}

struct RateLimiter {
    rate: u32,
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    fn new(rate: u32) -> Self {
        Self {
            rate: rate.max(1),
            tokens: f64::from(rate.max(1)),
            last_refill: Instant::now(),
        }
    }

    fn consume(&mut self, count: u32) -> Result<()> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * f64::from(self.rate)).min(f64::from(self.rate));
        self.last_refill = now;
        if self.tokens < f64::from(count) {
            return Err(ControllerError::new(
                ErrorCode::RateLimited,
                format!(
                    "operation needs {count} input events; limit is {} events per second",
                    self.rate
                ),
            )
            .retryable(true));
        }
        self.tokens -= f64::from(count);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_partially_visible_geometry() {
        assert_eq!(
            clip_geometry(
                Geometry {
                    x: -10,
                    y: 10,
                    width: 30,
                    height: 40,
                },
                100,
                100,
            )
            .expect("visible region"),
            Geometry {
                x: 0,
                y: 10,
                width: 20,
                height: 40,
            }
        );
    }

    #[test]
    fn coalesces_overlapping_and_edge_touching_rectangles() {
        let mut rectangles = vec![
            Geometry {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
            Geometry {
                x: 10,
                y: 0,
                width: 5,
                height: 10,
            },
            Geometry {
                x: 4,
                y: 4,
                width: 2,
                height: 20,
            },
        ];
        coalesce_rectangles(&mut rectangles);
        assert_eq!(
            rectangles,
            vec![Geometry {
                x: 0,
                y: 0,
                width: 15,
                height: 24,
            }]
        );
    }

    #[test]
    fn keeps_separated_rectangles_as_patches() {
        let mut rectangles = vec![
            Geometry {
                x: 20,
                y: 20,
                width: 2,
                height: 2,
            },
            Geometry {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
        ];
        coalesce_rectangles(&mut rectangles);
        assert_eq!(rectangles.len(), 2);
        assert_eq!(rectangles[0].x, 0);
    }

    #[test]
    fn delta_falls_back_above_patch_limit_or_at_area_threshold() {
        let bounds = Geometry {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let sixteen = (0..16)
            .map(|index| Geometry {
                x: index * 2,
                y: 0,
                width: 1,
                height: 1,
            })
            .collect::<Vec<_>>();
        assert!(!delta_should_use_full(&sixteen, bounds));
        let mut seventeen = sixteen;
        seventeen.push(Geometry {
            x: 40,
            y: 0,
            width: 1,
            height: 1,
        });
        assert!(delta_should_use_full(&seventeen, bounds));
        assert!(delta_should_use_full(
            &[Geometry {
                x: 0,
                y: 0,
                width: 60,
                height: 100,
            }],
            bounds
        ));
    }

    #[test]
    fn intersects_and_clips_damage_to_capture_bounds() {
        assert_eq!(
            intersect_geometry(
                Geometry {
                    x: -5,
                    y: 5,
                    width: 20,
                    height: 20,
                },
                Geometry {
                    x: 0,
                    y: 0,
                    width: 10,
                    height: 10,
                },
            ),
            Some(Geometry {
                x: 0,
                y: 5,
                width: 10,
                height: 5,
            })
        );
    }

    #[test]
    fn rate_limiter_rejects_burst_over_capacity() {
        let mut limiter = RateLimiter::new(2);
        assert!(limiter.consume(2).is_ok());
        assert_eq!(
            limiter.consume(1).expect_err("limited").code,
            ErrorCode::RateLimited
        );
    }
}

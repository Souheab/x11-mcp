//! Optional AT-SPI semantic desktop controller.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use atspi::{
    AccessibilityConnection, Event, FocusEvents, Interface, ObjectEvents, WindowEvents,
    proxy::{
        accessible::{AccessibleProxy, ObjectRefExt as _},
        proxy_ext::ProxyExt as _,
    },
};
use futures_lite::StreamExt as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};
use x11_controller::{ControllerError, ErrorCode, Geometry, ObserveAfter, StateGuard, WindowInfo};

const DEFAULT_MAX_DEPTH: u8 = 8;
const DEFAULT_MAX_NODES: usize = 500;
const MAX_DEPTH: u8 = 32;
const MAX_NODES: usize = 2_000;
const DEFAULT_TEXT_LIMIT: usize = 1_024;
const MAX_TEXT_LIMIT: usize = 4_096;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccessibilityMode {
    #[default]
    Auto,
    Disabled,
    Required,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AccessibilityInfo {
    pub available: bool,
    pub generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccessibilityRoot {
    #[default]
    Desktop,
    Window {
        window_ref: String,
    },
    Element {
        element_ref: String,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ElementSelector {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name_contains: Option<String>,
    pub states_all: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AccessibilitySnapshotRequest {
    pub root: AccessibilityRoot,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<ElementSelector>,
    pub max_depth: u8,
    pub max_nodes: usize,
    pub include_text: bool,
    pub text_limit: usize,
}

impl Default for AccessibilitySnapshotRequest {
    fn default() -> Self {
        Self {
            root: AccessibilityRoot::Desktop,
            selector: None,
            max_depth: DEFAULT_MAX_DEPTH,
            max_nodes: DEFAULT_MAX_NODES,
            include_text: false,
            text_limit: DEFAULT_TEXT_LIMIT,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AccessibleValue {
    pub current: f64,
    pub minimum: f64,
    pub maximum: f64,
    pub increment: f64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AccessibleNode {
    pub element_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_ref: Option<String>,
    pub role: String,
    pub name: String,
    pub description: String,
    pub states: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Geometry>,
    pub interfaces: Vec<String>,
    pub actions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<AccessibleValue>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AccessibilitySnapshot {
    pub generation: u64,
    pub truncated: bool,
    pub nodes: Vec<AccessibleNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ElementAction {
    Invoke {
        #[serde(default)]
        name: Option<String>,
    },
    Focus,
    SetText {
        text: String,
    },
    SetValue {
        value: f64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AccessibilityActionRequest {
    pub element_ref: String,
    #[serde(flatten)]
    pub action: ElementAction,
    #[serde(default)]
    pub guard: StateGuard,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observe_after: Option<ObserveAfter>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AccessibilityActionResult {
    pub ok: bool,
    pub element_ref: String,
    pub accessibility_generation: u64,
}

#[derive(Default)]
struct State {
    generation: u64,
    next_ref: u64,
    topology: Vec<String>,
    by_key: HashMap<String, String>,
    objects: HashMap<String, atspi::ObjectRefOwned>,
    bus_pids: HashMap<String, Option<u32>>,
}

pub struct AccessibilityController {
    connection: AccessibilityConnection,
    state: Arc<Mutex<State>>,
    event_sequence: Arc<AtomicU64>,
    event_notify: Arc<Notify>,
    connected: Arc<AtomicBool>,
    event_driven: bool,
}

impl AccessibilityController {
    /// Connect to the current session's AT-SPI bus.
    ///
    /// # Errors
    ///
    /// Returns an accessibility error when the bus or registry is unavailable.
    pub async fn connect() -> Result<Self, ControllerError> {
        let connection = AccessibilityConnection::new()
            .await
            .map_err(accessibility_error)?;
        connection
            .root_accessible_on_registry()
            .await
            .map_err(accessibility_error)?;
        let event_driven = register_events(&connection).await;
        let state = Arc::new(Mutex::new(State {
            generation: 1,
            next_ref: 1,
            ..State::default()
        }));
        let event_sequence = Arc::new(AtomicU64::new(0));
        let event_notify = Arc::new(Notify::new());
        let connected = Arc::new(AtomicBool::new(true));
        if event_driven {
            spawn_event_monitor(
                connection.clone(),
                state.clone(),
                event_sequence.clone(),
                event_notify.clone(),
                connected.clone(),
            );
        }
        Ok(Self {
            connection,
            state,
            event_sequence,
            event_notify,
            connected,
            event_driven,
        })
    }

    pub async fn info(&self) -> AccessibilityInfo {
        let state = self.state.lock().await;
        AccessibilityInfo {
            available: self.connected.load(Ordering::Acquire),
            generation: state.generation,
            reason: (!self.connected.load(Ordering::Acquire))
                .then(|| "AT-SPI event stream disconnected".to_owned()),
        }
    }

    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    #[must_use]
    pub const fn event_driven(&self) -> bool {
        self.event_driven
    }

    #[must_use]
    pub fn event_sequence(&self) -> u64 {
        self.event_sequence.load(Ordering::Acquire)
    }

    pub async fn wait_for_event(&self, since: u64, duration: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + duration;
        loop {
            let notified = self.event_notify.notified();
            if self.event_sequence() != since {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.event_sequence() != since;
            }
        }
    }

    /// Build a bounded, flat accessibility snapshot.
    ///
    /// # Errors
    ///
    /// Returns validation, stale-element, or AT-SPI transport errors.
    pub async fn snapshot(
        &self,
        request: AccessibilitySnapshotRequest,
        windows: &[WindowInfo],
    ) -> Result<AccessibilitySnapshot, ControllerError> {
        validate_snapshot_request(&request)?;
        let roots = self.resolve_roots(&request.root).await?;
        let mut queue = roots
            .into_iter()
            .map(|root| (root, None, 0_u8))
            .collect::<VecDeque<_>>();
        let mut nodes = Vec::new();
        let mut topology = Vec::new();
        let mut truncated = false;

        while let Some((object, parent_ref, depth)) = queue.pop_front() {
            if nodes.len() >= request.max_nodes {
                truncated = true;
                break;
            }
            let key = object_key(&object)?;
            let process_id = self.process_id(object.name_as_str()).await;
            let element_ref = self.ensure_ref(&key, object.clone()).await;
            topology.push(format!(
                "{key}|{}",
                parent_ref.as_deref().unwrap_or_default()
            ));
            let proxy = object
                .as_accessible_proxy(self.connection.connection())
                .await
                .map_err(accessibility_error)?;
            let node = self
                .read_node(
                    &proxy,
                    element_ref.clone(),
                    parent_ref,
                    request.include_text,
                    request.text_limit,
                    process_id,
                    windows,
                )
                .await?;
            if depth < request.max_depth {
                match proxy.get_children().await {
                    Ok(children) => {
                        for child in children.into_iter().filter(|child| !child.is_null()) {
                            queue.push_back((child, Some(element_ref.clone()), depth + 1));
                        }
                    }
                    Err(error) => {
                        tracing::debug!(%error, element_ref, "could not enumerate accessible children");
                    }
                }
            } else if proxy.child_count().await.unwrap_or_default() > 0 {
                truncated = true;
            }
            nodes.push(node);
        }

        topology.sort();
        let generation = {
            let mut state = self.state.lock().await;
            if state.topology != topology {
                if !state.topology.is_empty() {
                    state.generation = state.generation.saturating_add(1);
                }
                state.topology = topology;
            }
            state.generation
        };

        if let AccessibilityRoot::Window { window_ref } = &request.root {
            nodes.retain(|node| node.window_ref.as_deref() == Some(window_ref));
        }
        if let Some(selector) = &request.selector {
            nodes.retain(|node| selector.matches(node));
        }
        Ok(AccessibilitySnapshot {
            generation,
            truncated,
            nodes,
        })
    }

    /// Validate the semantic portion of a state guard without mutating an element.
    ///
    /// # Errors
    ///
    /// Returns a retryable precondition error when the generation is absent or stale.
    pub async fn validate_guard(&self, guard: &StateGuard) -> Result<(), ControllerError> {
        let expected = guard.accessibility_generation.ok_or_else(|| {
            ControllerError::new(
                ErrorCode::PreconditionFailed,
                "semantic actions require guard.accessibility_generation",
            )
            .retryable(true)
        })?;
        let generation = self.state.lock().await.generation;
        if generation != expected {
            return Err(accessibility_generation_error(expected, generation));
        }
        Ok(())
    }

    /// Execute one guarded semantic action.
    ///
    /// # Errors
    ///
    /// Returns stale-element, precondition, unsupported-interface, or transport errors.
    pub async fn action(
        &self,
        request: &AccessibilityActionRequest,
    ) -> Result<AccessibilityActionResult, ControllerError> {
        let expected_generation = if request.guard.prevalidated {
            None
        } else {
            Some(request.guard.accessibility_generation.ok_or_else(|| {
                ControllerError::new(
                    ErrorCode::PreconditionFailed,
                    "semantic actions require guard.accessibility_generation",
                )
                .retryable(true)
            })?)
        };
        let (generation, object) = {
            let state = self.state.lock().await;
            let object = state
                .objects
                .get(&request.element_ref)
                .cloned()
                .ok_or_else(|| {
                    ControllerError::new(
                        ErrorCode::StaleElement,
                        format!(
                            "unknown or stale element reference: {}",
                            request.element_ref
                        ),
                    )
                    .retryable(true)
                })?;
            (state.generation, object)
        };
        if let Some(expected_generation) = expected_generation
            && generation != expected_generation
        {
            return Err(accessibility_generation_error(
                expected_generation,
                generation,
            ));
        }

        let proxy = object
            .as_accessible_proxy(self.connection.connection())
            .await
            .map_err(|error| stale_or_accessibility(&request.element_ref, error))?;
        let proxies = proxy.proxies().await.map_err(accessibility_error)?;
        let ok = match &request.action {
            ElementAction::Invoke { name } => {
                let action = proxies.action().await.map_err(accessibility_error)?;
                let actions = action.get_actions().await.map_err(accessibility_error)?;
                let index = name.as_ref().map_or(Some(0), |wanted| {
                    actions
                        .iter()
                        .position(|candidate| candidate.name.eq_ignore_ascii_case(wanted))
                        .and_then(|index| i32::try_from(index).ok())
                });
                let index = index.ok_or_else(|| {
                    ControllerError::new(
                        ErrorCode::InvalidInput,
                        format!("element does not expose action {name:?}"),
                    )
                })?;
                action.do_action(index).await.map_err(accessibility_error)?
            }
            ElementAction::Focus => proxies
                .component()
                .await
                .map_err(accessibility_error)?
                .grab_focus()
                .await
                .map_err(accessibility_error)?,
            ElementAction::SetText { text } => proxies
                .editable_text()
                .await
                .map_err(accessibility_error)?
                .set_text_contents(text)
                .await
                .map_err(accessibility_error)?,
            ElementAction::SetValue { value } => {
                if !value.is_finite() {
                    return Err(ControllerError::new(
                        ErrorCode::InvalidInput,
                        "semantic value must be finite",
                    ));
                }
                proxies
                    .value()
                    .await
                    .map_err(accessibility_error)?
                    .set_current_value(*value)
                    .await
                    .map_err(accessibility_error)?;
                true
            }
        };
        Ok(AccessibilityActionResult {
            ok,
            element_ref: request.element_ref.clone(),
            accessibility_generation: generation,
        })
    }

    async fn resolve_roots(
        &self,
        root: &AccessibilityRoot,
    ) -> Result<Vec<atspi::ObjectRefOwned>, ControllerError> {
        match root {
            AccessibilityRoot::Desktop | AccessibilityRoot::Window { .. } => self
                .connection
                .root_accessible_on_registry()
                .await
                .map_err(accessibility_error)?
                .get_children()
                .await
                .map(|children| {
                    children
                        .into_iter()
                        .filter(|child| !child.is_null())
                        .collect()
                })
                .map_err(accessibility_error),
            AccessibilityRoot::Element { element_ref } => {
                let state = self.state.lock().await;
                state
                    .objects
                    .get(element_ref)
                    .cloned()
                    .map(|element| vec![element])
                    .ok_or_else(|| {
                        ControllerError::new(
                            ErrorCode::StaleElement,
                            format!("unknown or stale element reference: {element_ref}"),
                        )
                        .retryable(true)
                    })
            }
        }
    }

    async fn ensure_ref(&self, key: &str, object: atspi::ObjectRefOwned) -> String {
        let mut state = self.state.lock().await;
        if let Some(existing) = state.by_key.get(key) {
            let existing = existing.clone();
            state.objects.insert(existing.clone(), object);
            return existing;
        }
        let element_ref = format!("e{}", state.next_ref);
        state.next_ref = state.next_ref.saturating_add(1);
        state.generation = state.generation.saturating_add(1);
        state.by_key.insert(key.to_owned(), element_ref.clone());
        state.objects.insert(element_ref.clone(), object);
        element_ref
    }

    async fn process_id(&self, bus_name: Option<&str>) -> Option<u32> {
        let bus_name = bus_name?;
        if let Some(cached) = self.state.lock().await.bus_pids.get(bus_name).copied() {
            return cached;
        }
        let process_id = async {
            let name = atspi::zbus::names::BusName::try_from(bus_name).ok()?;
            let proxy = atspi::zbus::fdo::DBusProxy::new(self.connection.connection())
                .await
                .ok()?;
            proxy.get_connection_unix_process_id(name).await.ok()
        }
        .await;
        self.state
            .lock()
            .await
            .bus_pids
            .insert(bus_name.to_owned(), process_id);
        process_id
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn read_node(
        &self,
        proxy: &AccessibleProxy<'_>,
        element_ref: String,
        parent_ref: Option<String>,
        include_text: bool,
        text_limit: usize,
        process_id: Option<u32>,
        windows: &[WindowInfo],
    ) -> Result<AccessibleNode, ControllerError> {
        let name = proxy.name().await.unwrap_or_default();
        let description = proxy.description().await.unwrap_or_default();
        let role = proxy
            .get_role()
            .await
            .map_or_else(|_| "unknown".to_owned(), |role| role.name().to_owned());
        let states = proxy
            .get_state()
            .await
            .map(|states| {
                states
                    .iter()
                    .filter_map(|state| serde_json::to_value(state).ok())
                    .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let interface_set = proxy.get_interfaces().await.unwrap_or_default();
        let interfaces = interface_set
            .iter()
            .filter_map(|interface| serde_json::to_value(interface).ok())
            .filter_map(|value| value.as_str().map(ToOwned::to_owned))
            .collect::<Vec<_>>();
        let proxies = proxy.proxies().await.ok();
        let actions = if interface_set.contains(Interface::Action) {
            if let Some(proxies) = proxies.as_ref() {
                if let Ok(action) = proxies.action().await {
                    action
                        .get_actions()
                        .await
                        .map(|actions| actions.into_iter().map(|action| action.name).collect())
                        .unwrap_or_default()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        let bounds = if interface_set.contains(Interface::Component) {
            if let Some(proxies) = proxies.as_ref() {
                if let Ok(component) = proxies.component().await {
                    component
                        .get_extents(atspi::CoordType::Screen)
                        .await
                        .ok()
                        .and_then(extents_to_geometry)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        let value = if interface_set.contains(Interface::Value) {
            if let Some(proxies) = proxies.as_ref() {
                if let Ok(value_proxy) = proxies.value().await {
                    let current = value_proxy.current_value().await.ok();
                    let minimum = value_proxy.minimum_value().await.ok();
                    let maximum = value_proxy.maximum_value().await.ok();
                    let increment = value_proxy.minimum_increment().await.ok();
                    match (current, minimum, maximum, increment) {
                        (Some(current), Some(minimum), Some(maximum), Some(increment)) => {
                            Some(AccessibleValue {
                                current,
                                minimum,
                                maximum,
                                increment,
                            })
                        }
                        _ => None,
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        let text = if include_text && interface_set.contains(Interface::Text) {
            if let Some(proxies) = proxies.as_ref() {
                if let Ok(text_proxy) = proxies.text().await {
                    let count = text_proxy
                        .character_count()
                        .await
                        .unwrap_or_default()
                        .max(0);
                    let end = count.min(i32::try_from(text_limit).unwrap_or(i32::MAX));
                    text_proxy.get_text(0, end).await.ok()
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        let window_ref = bounds.and_then(|bounds| associate_window(bounds, process_id, windows));
        Ok(AccessibleNode {
            element_ref,
            parent_ref,
            window_ref,
            role,
            name,
            description,
            states,
            bounds,
            interfaces,
            actions,
            text,
            value,
        })
    }
}

async fn register_events(connection: &AccessibilityConnection) -> bool {
    let object = connection.register_event::<ObjectEvents>().await;
    let focus = connection.register_event::<FocusEvents>().await;
    let window = connection.register_event::<WindowEvents>().await;
    for error in [
        object.as_ref().err(),
        focus.as_ref().err(),
        window.as_ref().err(),
    ]
    .into_iter()
    .flatten()
    {
        tracing::warn!(%error, "AT-SPI event registration failed; semantic waits will poll");
    }
    object.is_ok() && focus.is_ok() && window.is_ok()
}

fn spawn_event_monitor(
    connection: AccessibilityConnection,
    state: Arc<Mutex<State>>,
    sequence: Arc<AtomicU64>,
    notify: Arc<Notify>,
    connected: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        let events = connection.event_stream();
        let mut events = std::pin::pin!(events);
        while let Some(event) = events.next().await {
            match event {
                Ok(event) => {
                    if matches!(
                        event,
                        Event::Object(ObjectEvents::ChildrenChanged(_)) | Event::Cache(_)
                    ) {
                        let mut state = state.lock().await;
                        state.generation = state.generation.saturating_add(1);
                        state.topology.clear();
                    }
                    sequence.fetch_add(1, Ordering::AcqRel);
                    notify.notify_waiters();
                }
                Err(error) => {
                    tracing::warn!(%error, "AT-SPI event stream disconnected");
                    break;
                }
            }
        }
        connected.store(false, Ordering::Release);
        sequence.fetch_add(1, Ordering::AcqRel);
        notify.notify_waiters();
    });
}

impl ElementSelector {
    #[must_use]
    pub fn matches(&self, node: &AccessibleNode) -> bool {
        if let Some(window_ref) = &self.window_ref
            && node.window_ref.as_ref() != Some(window_ref)
        {
            return false;
        }
        if let Some(role) = &self.role
            && !node.role.eq_ignore_ascii_case(role)
        {
            return false;
        }
        if let Some(name) = &self.name_contains
            && !node.name.to_lowercase().contains(&name.to_lowercase())
        {
            return false;
        }
        if !self.states_all.iter().all(|state| {
            node.states
                .iter()
                .any(|value| value.eq_ignore_ascii_case(state))
        }) {
            return false;
        }
        if let Some(action) = &self.action
            && !node
                .actions
                .iter()
                .any(|value| value.eq_ignore_ascii_case(action))
        {
            return false;
        }
        true
    }
}

fn validate_snapshot_request(
    request: &AccessibilitySnapshotRequest,
) -> Result<(), ControllerError> {
    if request.max_depth > MAX_DEPTH {
        return Err(ControllerError::new(
            ErrorCode::InvalidInput,
            format!("max_depth cannot exceed {MAX_DEPTH}"),
        ));
    }
    if request.max_nodes == 0 || request.max_nodes > MAX_NODES {
        return Err(ControllerError::new(
            ErrorCode::InvalidInput,
            format!("max_nodes must be between 1 and {MAX_NODES}"),
        ));
    }
    if request.text_limit > MAX_TEXT_LIMIT {
        return Err(ControllerError::new(
            ErrorCode::InvalidInput,
            format!("text_limit cannot exceed {MAX_TEXT_LIMIT}"),
        ));
    }
    Ok(())
}

fn object_key(object: &atspi::ObjectRefOwned) -> Result<String, ControllerError> {
    let name = object.name_as_str().ok_or_else(|| {
        ControllerError::new(ErrorCode::StaleElement, "AT-SPI returned a null object")
            .retryable(true)
    })?;
    Ok(format!("{name}|{}", object.path_as_str()))
}

fn extents_to_geometry((x, y, width, height): (i32, i32, i32, i32)) -> Option<Geometry> {
    Some(Geometry {
        x,
        y,
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
    })
    .filter(|bounds| bounds.width > 0 && bounds.height > 0)
}

fn associate_window(
    bounds: Geometry,
    process_id: Option<u32>,
    windows: &[WindowInfo],
) -> Option<String> {
    let overlapping = windows
        .iter()
        .filter(|window| window.mapped && geometries_overlap(bounds, window.geometry))
        .collect::<Vec<_>>();
    if let Some(process_id) = process_id {
        let matching = overlapping
            .iter()
            .copied()
            .filter(|window| window.pid == Some(process_id))
            .collect::<Vec<_>>();
        match matching.as_slice() {
            [window] => return Some(window.window_ref.clone()),
            [_, ..] => return None,
            [] => {}
        }
    }
    match overlapping.as_slice() {
        [window] => Some(window.window_ref.clone()),
        _ => None,
    }
}

fn geometries_overlap(left: Geometry, right: Geometry) -> bool {
    let left_right = i64::from(left.x) + i64::from(left.width);
    let right_right = i64::from(right.x) + i64::from(right.width);
    let left_bottom = i64::from(left.y) + i64::from(left.height);
    let right_bottom = i64::from(right.y) + i64::from(right.height);
    i64::from(left.x) < right_right
        && i64::from(right.x) < left_right
        && i64::from(left.y) < right_bottom
        && i64::from(right.y) < left_bottom
}

fn accessibility_generation_error(expected: u64, generation: u64) -> ControllerError {
    ControllerError::new(
        ErrorCode::PreconditionFailed,
        "accessibility topology changed since the guarded snapshot",
    )
    .retryable(true)
    .with_details(serde_json::json!({
        "expected_accessibility_generation": expected,
        "accessibility_generation": generation,
    }))
}

fn stale_or_accessibility(element_ref: &str, error: atspi::AtspiError) -> ControllerError {
    let message = error.to_string();
    if message.contains("UnknownObject") || message.contains("NoReply") {
        ControllerError::new(
            ErrorCode::StaleElement,
            format!("element {element_ref} is stale: {message}"),
        )
        .retryable(true)
    } else {
        accessibility_error(error)
    }
}

fn accessibility_error(error: impl std::fmt::Display) -> ControllerError {
    ControllerError::new(ErrorCode::Accessibility, error.to_string()).retryable(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11_controller::WindowClass;

    fn node() -> AccessibleNode {
        AccessibleNode {
            element_ref: "e1".to_owned(),
            parent_ref: None,
            window_ref: Some("window-1".to_owned()),
            role: "push button".to_owned(),
            name: "Save document".to_owned(),
            description: String::new(),
            states: vec!["enabled".to_owned(), "focusable".to_owned()],
            bounds: None,
            interfaces: vec!["action".to_owned()],
            actions: vec!["click".to_owned()],
            text: None,
            value: None,
        }
    }

    fn window(window_ref: &str, pid: u32, geometry: Geometry) -> WindowInfo {
        WindowInfo {
            window_ref: window_ref.to_owned(),
            xid: "0x00000001".to_owned(),
            title: String::new(),
            class: WindowClass {
                instance: None,
                name: None,
            },
            pid: Some(pid),
            desktop: None,
            geometry,
            mapped: true,
        }
    }

    #[test]
    fn selector_matches_window_role_name_states_and_action() {
        let selector = ElementSelector {
            window_ref: Some("window-1".to_owned()),
            role: Some("PUSH BUTTON".to_owned()),
            name_contains: Some("save".to_owned()),
            states_all: vec!["ENABLED".to_owned()],
            action: Some("CLICK".to_owned()),
        };
        assert!(selector.matches(&node()));
        assert!(
            !ElementSelector {
                states_all: vec!["checked".to_owned()],
                ..selector
            }
            .matches(&node())
        );
    }

    #[test]
    fn association_prefers_pid_and_rejects_ambiguity() {
        let bounds = Geometry {
            x: 20,
            y: 20,
            width: 10,
            height: 10,
        };
        let geometry = Geometry {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let windows = vec![
            window("window-1", 10, geometry),
            window("window-2", 20, geometry),
        ];
        assert_eq!(
            associate_window(bounds, Some(20), &windows).as_deref(),
            Some("window-2")
        );
        assert_eq!(associate_window(bounds, None, &windows), None);

        let duplicate_pid = vec![
            window("window-1", 20, geometry),
            window("window-2", 20, geometry),
        ];
        assert_eq!(associate_window(bounds, Some(20), &duplicate_pid), None);
    }

    #[test]
    fn validates_snapshot_limits() {
        assert_eq!(
            validate_snapshot_request(&AccessibilitySnapshotRequest {
                max_depth: MAX_DEPTH + 1,
                ..AccessibilitySnapshotRequest::default()
            })
            .expect_err("depth limit")
            .code,
            ErrorCode::InvalidInput
        );
        assert_eq!(
            validate_snapshot_request(&AccessibilitySnapshotRequest {
                max_nodes: MAX_NODES + 1,
                ..AccessibilitySnapshotRequest::default()
            })
            .expect_err("node limit")
            .code,
            ErrorCode::InvalidInput
        );
        assert_eq!(
            validate_snapshot_request(&AccessibilitySnapshotRequest {
                text_limit: MAX_TEXT_LIMIT + 1,
                ..AccessibilitySnapshotRequest::default()
            })
            .expect_err("text limit")
            .code,
            ErrorCode::InvalidInput
        );
    }
}

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct ControllerConfig {
    pub display: String,
    pub allow_window_classes: Vec<String>,
    pub max_input_events_per_second: u32,
    pub paste_chord: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Capabilities {
    pub display: String,
    pub screen: ScreenInfo,
    pub monitors: Vec<MonitorInfo>,
    pub extensions: BTreeMap<String, bool>,
    pub ewmh: bool,
    pub security: SecurityInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScreenInfo {
    pub width: u16,
    pub height: u16,
    pub depth: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MonitorInfo {
    pub name: String,
    pub primary: bool,
    pub geometry: Geometry,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SecurityInfo {
    pub window_allowlist_enabled: bool,
    pub input_events_per_second: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WindowClass {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WindowInfo {
    pub window_ref: String,
    pub xid: String,
    pub title: String,
    pub class: WindowClass,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop: Option<u32>,
    pub geometry: Geometry,
    pub mapped: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WindowSelector {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title_contains: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ListWindowsRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<WindowSelector>,
    pub include_unmapped: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WindowList {
    pub desktop_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_window: Option<String>,
    pub windows: Vec<WindowInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "coordinate_space",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Position {
    Screen { x: i32, y: i32 },
    Window { window_ref: String, x: i32, y: i32 },
    WindowRelative { window_ref: String, x: f64, y: f64 },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObserveTarget {
    #[default]
    Desktop,
    Window {
        window_ref: String,
    },
    Region {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ObserveRequest {
    pub target: ObserveTarget,
    pub include_windows: bool,
}

impl Default for ObserveRequest {
    fn default() -> Self {
        Self {
            target: ObserveTarget::Desktop,
            include_windows: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PointerInfo {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ObservationMetadata {
    pub frame_id: u64,
    pub desktop_generation: u64,
    pub bounds: Geometry,
    pub pointer: PointerInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_window: Option<String>,
    pub windows: Vec<WindowInfo>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Observation {
    #[serde(flatten)]
    pub metadata: ObservationMetadata,
    #[serde(skip)]
    #[schemars(skip)]
    pub png: Vec<u8>,
    #[serde(skip)]
    #[schemars(skip)]
    pub signature: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ObserveAfter {
    pub quiet_ms: u64,
    pub timeout_ms: u64,
    pub require_change: bool,
}

impl Default for ObserveAfter {
    fn default() -> Self {
        Self {
            quiet_ms: 150,
            timeout_ms: 3_000,
            require_change: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct FocusWindowRequest {
    pub window_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observe_after: Option<ObserveAfter>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MovePointerRequest {
    pub position: Position,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observe_after: Option<ObserveAfter>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ClickRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<Position>,
    pub button: u8,
    pub count: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observe_after: Option<ObserveAfter>,
}

impl Default for ClickRequest {
    fn default() -> Self {
        Self {
            position: None,
            button: 1,
            count: 1,
            observe_after: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DragRequest {
    pub from: Position,
    pub to: Position,
    pub button: u8,
    pub duration_ms: u64,
    pub steps: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observe_after: Option<ObserveAfter>,
}

impl Default for DragRequest {
    fn default() -> Self {
        Self {
            from: Position::Screen { x: 0, y: 0 },
            to: Position::Screen { x: 0, y: 0 },
            button: 1,
            duration_ms: 300,
            steps: 10,
            observe_after: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ScrollRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<Position>,
    pub dx: i32,
    pub dy: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observe_after: Option<ObserveAfter>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeyMode {
    #[default]
    Press,
    Down,
    Up,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct KeyRequest {
    pub keys: Vec<String>,
    pub mode: KeyMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observe_after: Option<ObserveAfter>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TextMethod {
    #[default]
    Auto,
    Keystrokes,
    Clipboard,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TypeTextRequest {
    pub text: String,
    pub method: TextMethod,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observe_after: Option<ObserveAfter>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum WindowAction {
    Move {
        x: i32,
        y: i32,
    },
    Resize {
        width: u32,
        height: u32,
    },
    MoveResize {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
    Minimize,
    Maximize,
    Restore,
    Close {
        #[serde(default)]
        force: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WindowActionRequest {
    pub window_ref: String,
    #[serde(flatten)]
    pub action: WindowAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observe_after: Option<ObserveAfter>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "condition", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaitCondition {
    Change {
        #[serde(default)]
        since_frame_id: Option<u64>,
    },
    Idle {
        #[serde(default = "default_quiet_ms")]
        quiet_ms: u64,
    },
    Window {
        selector: WindowSelector,
    },
    Focus {
        window_ref: String,
    },
    WindowClosed {
        window_ref: String,
    },
}

const fn default_quiet_ms() -> u64 {
    150
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitRequest {
    pub condition: WaitCondition,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_true")]
    pub observe: bool,
}

const fn default_timeout_ms() -> u64 {
    3_000
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ActionResult {
    pub ok: bool,
    pub settled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation: Option<Observation>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WaitResult {
    pub matched: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation: Option<Observation>,
}

impl WindowSelector {
    #[must_use]
    pub fn matches(&self, window: &WindowInfo) -> bool {
        if let Some(title) = &self.title_contains
            && !window.title.to_lowercase().contains(&title.to_lowercase())
        {
            return false;
        }
        if let Some(class) = &self.class {
            let wanted = class.to_lowercase();
            let matches = window
                .class
                .instance
                .as_deref()
                .is_some_and(|value| value.to_lowercase() == wanted)
                || window
                    .class
                    .name
                    .as_deref()
                    .is_some_and(|value| value.to_lowercase() == wanted);
            if !matches {
                return false;
            }
        }
        if let Some(pid) = self.pid
            && window.pid != Some(pid)
        {
            return false;
        }
        true
    }
}

mod actor;
mod capture;
mod error;
mod keyboard;
mod types;

use std::sync::{Arc, atomic::AtomicBool};

use async_trait::async_trait;

pub use actor::ControllerHandle;
pub use error::{ControllerError, ErrorCode, Result};
pub use types::*;

#[async_trait]
pub trait DesktopController: Send + Sync {
    async fn capabilities(&self) -> Result<Capabilities>;
    async fn observe(&self, request: ObserveRequest) -> Result<Observation>;
    async fn list_windows(&self, request: ListWindowsRequest) -> Result<WindowList>;
    async fn focus_window(&self, request: FocusWindowRequest) -> Result<ActionResult>;
    async fn move_pointer(&self, request: MovePointerRequest) -> Result<ActionResult>;
    async fn click(&self, request: ClickRequest) -> Result<ActionResult>;
    async fn drag(&self, request: DragRequest) -> Result<ActionResult>;
    async fn scroll(&self, request: ScrollRequest) -> Result<ActionResult>;
    async fn key(&self, request: KeyRequest) -> Result<ActionResult>;
    async fn type_text(&self, request: TypeTextRequest) -> Result<ActionResult>;
    async fn window_action(&self, request: WindowActionRequest) -> Result<ActionResult>;
    async fn wait_for(&self, request: WaitRequest) -> Result<WaitResult>;
    async fn validate_state_guard(
        &self,
        guard: StateGuard,
        require_frame: bool,
        include_current_pointer: bool,
        positions: Vec<Position>,
    ) -> Result<()>;
    async fn validate_window_allowed(&self, window_ref: String) -> Result<()>;
    async fn release_input(&self) -> Result<()>;
}

/// Connects a controller actor to one X11 display.
///
/// # Errors
///
/// Returns an error when the display cannot be opened, its initial capabilities cannot be read,
/// or a configured window-class glob is invalid.
pub fn connect(
    config: ControllerConfig,
    emergency_stop: Arc<AtomicBool>,
) -> Result<ControllerHandle> {
    ControllerHandle::connect(config, emergency_stop)
}

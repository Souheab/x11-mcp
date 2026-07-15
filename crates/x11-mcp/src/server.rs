use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rmcp::{
    ServerHandler,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, ErrorData},
    tool, tool_handler, tool_router,
};
use serde::Serialize;
use serde_json::Value;
use x11_controller::{
    ClickRequest, ControllerError, DesktopController, DragRequest, FocusWindowRequest, KeyRequest,
    ListWindowsRequest, MovePointerRequest, ObserveRequest, ScrollRequest, TypeTextRequest,
    WaitRequest, WindowActionRequest,
};

#[derive(Clone)]
pub struct X11McpServer {
    controller: Arc<dyn DesktopController>,
}

impl X11McpServer {
    pub fn new(controller: Arc<dyn DesktopController>) -> Self {
        Self { controller }
    }
}

#[tool_router]
impl X11McpServer {
    #[tool(
        name = "x11.get_capabilities",
        description = "Report the target X11 display, geometry, monitors, extensions, and active safety controls",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn get_capabilities(&self) -> std::result::Result<CallToolResult, ErrorData> {
        Ok(to_tool_result(self.controller.capabilities().await, None))
    }

    #[tool(
        name = "x11.observe",
        description = "Capture the desktop, a visible window crop, or a screen region as PNG with structured desktop metadata",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn observe(
        &self,
        Parameters(request): Parameters<ObserveRequest>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let result = self.controller.observe(request).await;
        Ok(match result {
            Ok(observation) => success_result(&observation, Some(&observation.png)),
            Err(error) => error_result(&error),
        })
    }

    #[tool(
        name = "x11.list_windows",
        description = "List X11 client windows with stable session references, titles, classes, PIDs, mapping state, and geometry",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn list_windows(
        &self,
        Parameters(request): Parameters<ListWindowsRequest>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        Ok(to_tool_result(
            self.controller.list_windows(request).await,
            None,
        ))
    }

    #[tool(
        name = "x11.focus_window",
        description = "Ask the window manager to focus a window",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn focus_window(
        &self,
        Parameters(request): Parameters<FocusWindowRequest>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let result = self.controller.focus_window(request).await;
        Ok(action_result(result))
    }

    #[tool(
        name = "x11.move_pointer",
        description = "Move the X11 pointer in screen or window coordinates",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn move_pointer(
        &self,
        Parameters(request): Parameters<MovePointerRequest>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        Ok(action_result(self.controller.move_pointer(request).await))
    }

    #[tool(
        name = "x11.click",
        description = "Move optionally and synthesize one or more X11 pointer clicks",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn click(
        &self,
        Parameters(request): Parameters<ClickRequest>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        Ok(action_result(self.controller.click(request).await))
    }

    #[tool(
        name = "x11.drag",
        description = "Drag between screen or window coordinates using XTEST",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn drag(
        &self,
        Parameters(request): Parameters<DragRequest>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        Ok(action_result(self.controller.drag(request).await))
    }

    #[tool(
        name = "x11.scroll",
        description = "Synthesize vertical and horizontal X11 scroll-button ticks",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn scroll(
        &self,
        Parameters(request): Parameters<ScrollRequest>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        Ok(action_result(self.controller.scroll(request).await))
    }

    #[tool(
        name = "x11.key",
        description = "Press, hold, or release a key chord using the active X11 keyboard map",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn key(
        &self,
        Parameters(request): Parameters<KeyRequest>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        Ok(action_result(self.controller.key(request).await))
    }

    #[tool(
        name = "x11.type_text",
        description = "Type text with mapped keystrokes or Unicode-capable clipboard paste and best-effort clipboard restoration",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn type_text(
        &self,
        Parameters(request): Parameters<TypeTextRequest>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        Ok(action_result(self.controller.type_text(request).await))
    }

    #[tool(
        name = "x11.window_action",
        description = "Move, resize, minimize, maximize, restore, or close a referenced X11 window",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn window_action(
        &self,
        Parameters(request): Parameters<WindowActionRequest>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        Ok(action_result(self.controller.window_action(request).await))
    }

    #[tool(
        name = "x11.wait_for",
        description = "Wait for a changed or idle frame, matching window, focus, or window closure without fixed sleeps",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn wait_for(
        &self,
        Parameters(request): Parameters<WaitRequest>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let result = self.controller.wait_for(request).await;
        Ok(match result {
            Ok(wait) => {
                let png = wait
                    .observation
                    .as_ref()
                    .map(|observation| observation.png.as_slice());
                success_result(&wait, png)
            }
            Err(error) => error_result(&error),
        })
    }
}

#[tool_handler(
    name = "x11-mcp",
    version = "0.1.0",
    instructions = "Controls one explicitly selected X11 display. Prefer isolated Xvfb or Xephyr displays, use stable window_ref values, and request observe_after when an action needs a settled screenshot."
)]
impl ServerHandler for X11McpServer {}

fn action_result(result: x11_controller::Result<x11_controller::ActionResult>) -> CallToolResult {
    match result {
        Ok(action) => {
            let png = action
                .observation
                .as_ref()
                .map(|observation| observation.png.as_slice());
            success_result(&action, png)
        }
        Err(error) => error_result(&error),
    }
}

fn to_tool_result<T: Serialize>(
    result: x11_controller::Result<T>,
    png: Option<&[u8]>,
) -> CallToolResult {
    match result {
        Ok(value) => success_result(&value, png),
        Err(error) => error_result(&error),
    }
}

fn success_result<T: Serialize>(value: &T, png: Option<&[u8]>) -> CallToolResult {
    match serde_json::to_value(value) {
        Ok(structured) => {
            let text = serde_json::to_string(&structured).unwrap_or_else(|_| "{}".to_owned());
            let mut content = vec![ContentBlock::text(text)];
            if let Some(png) = png {
                content.push(ContentBlock::image(STANDARD.encode(png), "image/png"));
            }
            let mut result = CallToolResult::success(content);
            result.structured_content = Some(structured);
            result
        }
        Err(error) => {
            let controller_error = ControllerError::new(
                x11_controller::ErrorCode::Internal,
                format!("serialize tool result: {error}"),
            );
            error_result(&controller_error)
        }
    }
}

fn error_result(error: &ControllerError) -> CallToolResult {
    let structured = serde_json::to_value(error).unwrap_or_else(|serialization_error| {
        Value::String(format!("could not serialize error: {serialization_error}"))
    });
    let mut result = CallToolResult::error(vec![ContentBlock::text(
        serde_json::to_string(&structured).unwrap_or_else(|_| error.to_string()),
    )]);
    result.structured_content = Some(structured);
    result
}

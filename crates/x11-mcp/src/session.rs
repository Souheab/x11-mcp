use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use atspi_controller::{
    AccessibilityActionRequest, AccessibilityActionResult, AccessibilityController,
    AccessibilityInfo, AccessibilityMode, AccessibilityRoot, AccessibilitySnapshot,
    AccessibilitySnapshotRequest, AccessibleNode, ElementSelector,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex, RwLock},
    time::{sleep, timeout},
};
use x11_controller::{
    ActionResult, AfterDelivery, Capabilities, ClickRequest, ControllerError, DesktopController,
    DragRequest, ErrorCode, FocusWindowRequest, KeyRequest, ListWindowsRequest, MovePointerRequest,
    Observation, ObservationDelivery, ObserveAfter, ObserveRequest, ObserveTarget, ScrollRequest,
    StateGuard, TypeTextRequest, WaitCondition, WaitRequest, WindowActionRequest, WindowInfo,
    WindowSelector,
};

use crate::applications::{
    AppList, ApplicationCapabilities, ApplicationLauncher, LaunchAppRequest, LaunchAppResult,
    ListAppsRequest,
};

const ACCESSIBILITY_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const WAIT_POLL: Duration = Duration::from_millis(50);
const MAX_BATCH_STEPS: usize = 64;
const MAX_TIMEOUT_MS: u64 = 60_000;

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SessionCapabilities {
    #[serde(flatten)]
    pub x11: Capabilities,
    pub accessibility: AccessibilityInfo,
    pub applications: ApplicationCapabilities,
    pub limits: SessionLimits,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SessionLimits {
    pub frame_history: usize,
    pub max_batch_steps: usize,
    pub max_semantic_nodes: usize,
}

#[derive(Clone)]
pub struct DesktopSession {
    controller: Arc<dyn DesktopController>,
    applications: ApplicationLauncher,
    accessibility: Arc<RwLock<Option<Arc<AccessibilityController>>>>,
    accessibility_reason: Arc<RwLock<Option<String>>>,
    accessibility_mode: AccessibilityMode,
    mutation: Arc<Mutex<()>>,
}

impl DesktopSession {
    pub async fn new(
        controller: Arc<dyn DesktopController>,
        applications: ApplicationLauncher,
        mode: AccessibilityMode,
    ) -> Result<Self, ControllerError> {
        let session = Self {
            controller,
            applications,
            accessibility: Arc::new(RwLock::new(None)),
            accessibility_reason: Arc::new(RwLock::new(None)),
            accessibility_mode: mode.clone(),
            mutation: Arc::new(Mutex::new(())),
        };
        if !matches!(mode, AccessibilityMode::Disabled) {
            match session.connect_accessibility().await {
                Ok(accessibility) => {
                    *session.accessibility.write().await = Some(accessibility);
                }
                Err(error) if matches!(mode, AccessibilityMode::Required) => return Err(error),
                Err(error) => {
                    *session.accessibility_reason.write().await = Some(error.message);
                }
            }
        }
        Ok(session)
    }

    pub async fn capabilities(&self) -> Result<SessionCapabilities, ControllerError> {
        let x11 = self.controller.capabilities().await?;
        let accessibility = if let Some(controller) = self.accessibility.read().await.as_ref() {
            controller.info().await
        } else {
            AccessibilityInfo {
                available: false,
                generation: 0,
                reason: self.accessibility_reason.read().await.clone(),
            }
        };
        Ok(SessionCapabilities {
            x11,
            accessibility,
            applications: self.applications.capabilities(),
            limits: SessionLimits {
                frame_history: 64,
                max_batch_steps: MAX_BATCH_STEPS,
                max_semantic_nodes: 2_000,
            },
        })
    }

    pub async fn observe(&self, request: ObserveRequest) -> Result<Observation, ControllerError> {
        self.controller.observe(request).await
    }

    pub async fn list_windows(
        &self,
        request: ListWindowsRequest,
    ) -> Result<x11_controller::WindowList, ControllerError> {
        self.controller.list_windows(request).await
    }

    pub async fn list_apps(&self, request: ListAppsRequest) -> Result<AppList, ControllerError> {
        self.applications.list_apps(request).await
    }

    pub async fn launch_app(
        &self,
        request: LaunchAppRequest,
    ) -> Result<LaunchAppResult, ControllerError> {
        let _guard = self.mutation.lock().await;
        self.applications.launch_app(request).await
    }

    pub async fn focus_window(
        &self,
        request: FocusWindowRequest,
    ) -> Result<ActionResult, ControllerError> {
        let _guard = self.mutation.lock().await;
        self.controller.focus_window(request).await
    }

    pub async fn move_pointer(
        &self,
        request: MovePointerRequest,
    ) -> Result<ActionResult, ControllerError> {
        let _guard = self.mutation.lock().await;
        self.controller.move_pointer(request).await
    }

    pub async fn click(&self, request: ClickRequest) -> Result<ActionResult, ControllerError> {
        let _guard = self.mutation.lock().await;
        self.controller.click(request).await
    }

    pub async fn drag(&self, request: DragRequest) -> Result<ActionResult, ControllerError> {
        let _guard = self.mutation.lock().await;
        self.controller.drag(request).await
    }

    pub async fn scroll(&self, request: ScrollRequest) -> Result<ActionResult, ControllerError> {
        let _guard = self.mutation.lock().await;
        self.controller.scroll(request).await
    }

    pub async fn key(&self, request: KeyRequest) -> Result<ActionResult, ControllerError> {
        let _guard = self.mutation.lock().await;
        self.controller.key(request).await
    }

    pub async fn type_text(
        &self,
        request: TypeTextRequest,
    ) -> Result<ActionResult, ControllerError> {
        let _guard = self.mutation.lock().await;
        self.controller.type_text(request).await
    }

    pub async fn window_action(
        &self,
        request: WindowActionRequest,
    ) -> Result<ActionResult, ControllerError> {
        let _guard = self.mutation.lock().await;
        self.controller.window_action(request).await
    }

    pub async fn accessibility_snapshot(
        &self,
        request: AccessibilitySnapshotRequest,
    ) -> Result<AccessibilitySnapshot, ControllerError> {
        let accessibility = self.ensure_accessibility().await?;
        let windows = self
            .controller
            .list_windows(ListWindowsRequest {
                selector: None,
                include_unmapped: true,
            })
            .await?;
        accessibility.snapshot(request, &windows.windows).await
    }

    pub async fn accessibility_action(
        &self,
        request: AccessibilityActionRequest,
    ) -> Result<AccessibilityActionOutput, ControllerError> {
        let _mutation = self.mutation.lock().await;
        self.controller
            .validate_state_guard(request.guard.clone(), false, false, Vec::new())
            .await?;
        let baseline = self
            .external_baseline(request.observe_after.as_ref())
            .await?;
        let result = self.execute_accessibility_action(&request).await?;
        let (observation, settled) = self
            .external_observe_after(request.observe_after.as_ref(), baseline)
            .await?;
        Ok(AccessibilityActionOutput {
            action: result,
            settled,
            observation,
        })
    }

    #[allow(clippy::too_many_lines)]
    pub async fn wait_for(
        &self,
        request: SessionWaitRequest,
    ) -> Result<SessionWaitResult, ControllerError> {
        validate_timeout(request.timeout_ms)?;
        let deadline = Instant::now() + Duration::from_millis(request.timeout_ms);
        match request.condition {
            SessionWaitCondition::FrameChanged {
                since_frame_id,
                target,
            } => {
                let result = self
                    .controller
                    .wait_for(WaitRequest {
                        condition: WaitCondition::Change {
                            since_frame_id,
                            target,
                        },
                        timeout_ms: request.timeout_ms,
                        observe: request.observe,
                    })
                    .await?;
                Ok(SessionWaitResult::observation(result.observation))
            }
            SessionWaitCondition::FrameIdle { quiet_ms, target } => {
                let result = self
                    .controller
                    .wait_for(WaitRequest {
                        condition: WaitCondition::Idle { quiet_ms, target },
                        timeout_ms: request.timeout_ms,
                        observe: request.observe,
                    })
                    .await?;
                Ok(SessionWaitResult::observation(result.observation))
            }
            SessionWaitCondition::WindowMatched { selector } => {
                let result = self
                    .controller
                    .wait_for(WaitRequest {
                        condition: WaitCondition::Window { selector },
                        timeout_ms: request.timeout_ms,
                        observe: request.observe,
                    })
                    .await?;
                Ok(SessionWaitResult::x11(result))
            }
            SessionWaitCondition::WindowState {
                window_ref,
                mapped,
                active,
                title_contains,
            } => {
                let result = self
                    .controller
                    .wait_for(WaitRequest {
                        condition: WaitCondition::WindowState {
                            window_ref,
                            mapped,
                            active,
                            title_contains,
                        },
                        timeout_ms: request.timeout_ms,
                        observe: request.observe,
                    })
                    .await?;
                Ok(SessionWaitResult::x11(result))
            }
            SessionWaitCondition::WindowClosed { window_ref } => {
                let result = self
                    .controller
                    .wait_for(WaitRequest {
                        condition: WaitCondition::WindowClosed { window_ref },
                        timeout_ms: request.timeout_ms,
                        observe: request.observe,
                    })
                    .await?;
                Ok(SessionWaitResult::x11(result))
            }
            SessionWaitCondition::ElementMatched { selector } => {
                let accessibility = self.ensure_accessibility().await?;
                loop {
                    let sequence = accessibility.event_sequence();
                    let snapshot = self
                        .accessibility_snapshot(AccessibilitySnapshotRequest {
                            selector: Some(selector.clone()),
                            ..AccessibilitySnapshotRequest::default()
                        })
                        .await?;
                    if let Some(element) = snapshot.nodes.into_iter().next() {
                        return self.wait_result(request.observe, None, Some(element)).await;
                    }
                    wait_accessibility_tick(&accessibility, sequence, deadline).await?;
                }
            }
            SessionWaitCondition::ElementState {
                element_ref,
                states_all,
                name_contains,
                text_contains,
                value,
            } => {
                let accessibility = self.ensure_accessibility().await?;
                loop {
                    let sequence = accessibility.event_sequence();
                    let snapshot = self
                        .accessibility_snapshot(AccessibilitySnapshotRequest {
                            root: AccessibilityRoot::Element {
                                element_ref: element_ref.clone(),
                            },
                            max_depth: 0,
                            max_nodes: 1,
                            include_text: text_contains.is_some(),
                            ..AccessibilitySnapshotRequest::default()
                        })
                        .await?;
                    if let Some(element) = snapshot.nodes.into_iter().next() {
                        let matches = states_all.iter().all(|wanted| {
                            element
                                .states
                                .iter()
                                .any(|state| state.eq_ignore_ascii_case(wanted))
                        }) && name_contains.as_ref().is_none_or(|wanted| {
                            element.name.to_lowercase().contains(&wanted.to_lowercase())
                        }) && text_contains.as_ref().is_none_or(|wanted| {
                            element
                                .text
                                .as_ref()
                                .is_some_and(|text| text.contains(wanted))
                        }) && value
                            .as_ref()
                            .is_none_or(|range| range.matches(element.value.as_ref()));
                        if matches {
                            return self.wait_result(request.observe, None, Some(element)).await;
                        }
                    }
                    wait_accessibility_tick(&accessibility, sequence, deadline).await?;
                }
            }
        }
    }

    pub async fn batch(&self, request: BatchRequest) -> Result<BatchResult, ControllerError> {
        if request.steps.is_empty() || request.steps.len() > MAX_BATCH_STEPS {
            return Err(ControllerError::new(
                ErrorCode::InvalidInput,
                format!("batch must contain between 1 and {MAX_BATCH_STEPS} steps"),
            ));
        }
        validate_timeout(request.timeout_ms)?;
        let _mutation = self.mutation.lock().await;
        let deadline = Instant::now() + Duration::from_millis(request.timeout_ms);
        let scope = batch_guard_scope(&request.steps);
        let validation = async {
            self.controller
                .validate_state_guard(
                    request.guard.clone(),
                    scope.require_frame,
                    scope.include_current_pointer,
                    scope.positions,
                )
                .await?;
            if scope.semantic {
                self.ensure_accessibility()
                    .await?
                    .validate_guard(&request.guard)
                    .await?;
            }
            Ok::<(), ControllerError>(())
        };
        match timeout(
            deadline.saturating_duration_since(Instant::now()),
            validation,
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(batch_error(error, 0, &[])),
            Err(_) => return Err(batch_error(batch_timeout_error(), 0, &[])),
        }
        let mut step_guard = request.guard.clone();
        step_guard.prevalidated = true;
        let baseline = match timeout(
            deadline.saturating_duration_since(Instant::now()),
            self.external_baseline(request.observe_after.as_ref()),
        )
        .await
        {
            Ok(Ok(baseline)) => baseline,
            Ok(Err(error)) => return Err(batch_error(error, 0, &[])),
            Err(_) => {
                return Err(batch_error(batch_timeout_error(), 0, &[]));
            }
        };
        let step_count = request.steps.len();
        let mut completed = Vec::with_capacity(step_count);

        for (index, step) in request.steps.into_iter().enumerate() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = self.controller.release_input().await;
                return Err(batch_error(batch_timeout_error(), index, &completed));
            }
            let result = timeout(
                remaining,
                self.execute_batch_step(step, &step_guard, deadline),
            )
            .await;
            match result {
                Ok(Ok(value)) => completed.push(BatchStepResult { index, value }),
                Ok(Err(error)) => {
                    let _ = self.controller.release_input().await;
                    return Err(batch_error(error, index, &completed));
                }
                Err(_) => {
                    let _ = self.controller.release_input().await;
                    return Err(batch_error(batch_timeout_error(), index, &completed));
                }
            }
        }
        if let Err(error) = self.controller.release_input().await {
            return Err(batch_error(error, step_count, &completed));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(batch_error(batch_timeout_error(), step_count, &completed));
        }
        let (observation, settled) = timeout(
            remaining,
            self.external_observe_after(request.observe_after.as_ref(), baseline),
        )
        .await
        .map_err(|_| batch_error(batch_timeout_error(), step_count, &completed))??;
        Ok(BatchResult {
            ok: true,
            steps: completed,
            settled,
            observation,
        })
    }

    async fn execute_batch_step(
        &self,
        step: BatchStep,
        guard: &StateGuard,
        deadline: Instant,
    ) -> Result<Value, ControllerError> {
        match step {
            BatchStep::FocusWindow { mut request } => {
                request.guard = guard.clone();
                request.observe_after = None;
                serde_value(self.controller.focus_window(request).await?)
            }
            BatchStep::MovePointer { mut request } => {
                request.guard = guard.clone();
                request.observe_after = None;
                serde_value(self.controller.move_pointer(request).await?)
            }
            BatchStep::Click { mut request } => {
                request.guard = guard.clone();
                request.observe_after = None;
                serde_value(self.controller.click(request).await?)
            }
            BatchStep::Drag { mut request } => {
                request.guard = guard.clone();
                request.observe_after = None;
                serde_value(self.controller.drag(request).await?)
            }
            BatchStep::Scroll { mut request } => {
                request.guard = guard.clone();
                request.observe_after = None;
                serde_value(self.controller.scroll(request).await?)
            }
            BatchStep::Key { mut request } => {
                request.guard = guard.clone();
                request.observe_after = None;
                serde_value(self.controller.key(request).await?)
            }
            BatchStep::TypeText { mut request } => {
                request.guard = guard.clone();
                request.observe_after = None;
                serde_value(self.controller.type_text(request).await?)
            }
            BatchStep::WindowAction { mut request } => {
                request.guard = guard.clone();
                request.observe_after = None;
                serde_value(self.controller.window_action(request).await?)
            }
            BatchStep::AccessibilityAction { mut request } => {
                request.guard = guard.clone();
                request.observe_after = None;
                serde_value(self.execute_accessibility_action(&request).await?)
            }
            BatchStep::LaunchApp { request } => {
                serde_value(self.applications.launch_app(request).await?)
            }
            BatchStep::WaitFor { mut request } => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                request.timeout_ms = request
                    .timeout_ms
                    .min(u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX));
                serde_value(self.wait_for(request).await?)
            }
        }
    }

    async fn execute_accessibility_action(
        &self,
        request: &AccessibilityActionRequest,
    ) -> Result<AccessibilityActionResult, ControllerError> {
        let accessibility = self.ensure_accessibility().await?;
        let capabilities = self.controller.capabilities().await?;
        if capabilities.security.window_allowlist_enabled {
            let windows = self
                .controller
                .list_windows(ListWindowsRequest {
                    selector: None,
                    include_unmapped: true,
                })
                .await?;
            let snapshot = accessibility
                .snapshot(
                    AccessibilitySnapshotRequest {
                        root: AccessibilityRoot::Element {
                            element_ref: request.element_ref.clone(),
                        },
                        max_depth: 0,
                        max_nodes: 1,
                        ..AccessibilitySnapshotRequest::default()
                    },
                    &windows.windows,
                )
                .await?;
            let window_ref = snapshot
                .nodes
                .first()
                .and_then(|node| node.window_ref.clone())
                .ok_or_else(|| {
                    ControllerError::new(
                        ErrorCode::AccessDenied,
                        "semantic element cannot be associated with an allowlisted window",
                    )
                })?;
            self.controller.validate_window_allowed(window_ref).await?;
        }
        accessibility.action(request).await
    }

    async fn ensure_accessibility(&self) -> Result<Arc<AccessibilityController>, ControllerError> {
        let existing = self.accessibility.read().await.clone();
        if let Some(controller) = existing {
            if controller.is_connected() {
                return Ok(controller);
            }
            *self.accessibility.write().await = None;
            *self.accessibility_reason.write().await =
                Some("AT-SPI connection was lost; reconnecting".to_owned());
        }
        if matches!(self.accessibility_mode, AccessibilityMode::Disabled) {
            return Err(ControllerError::new(
                ErrorCode::UnsupportedCapability,
                "accessibility support is disabled",
            ));
        }
        match self.connect_accessibility().await {
            Ok(controller) => {
                *self.accessibility.write().await = Some(controller.clone());
                *self.accessibility_reason.write().await = None;
                Ok(controller)
            }
            Err(error) => {
                *self.accessibility_reason.write().await = Some(error.message.clone());
                Err(ControllerError::new(
                    ErrorCode::UnsupportedCapability,
                    format!("AT-SPI is unavailable: {}", error.message),
                )
                .retryable(true))
            }
        }
    }

    async fn connect_accessibility(&self) -> Result<Arc<AccessibilityController>, ControllerError> {
        timeout(
            ACCESSIBILITY_CONNECT_TIMEOUT,
            AccessibilityController::connect(),
        )
        .await
        .map_err(|_| {
            ControllerError::new(
                ErrorCode::Accessibility,
                "timed out connecting to the AT-SPI bus",
            )
            .retryable(true)
        })?
        .map(Arc::new)
    }

    async fn external_baseline(
        &self,
        after: Option<&ObserveAfter>,
    ) -> Result<Option<Observation>, ControllerError> {
        let Some(after) = after else {
            return Ok(None);
        };
        if after.require_change || matches!(after.delivery, AfterDelivery::Delta) {
            self.controller
                .observe(ObserveRequest {
                    target: after.target.clone(),
                    include_windows: after.include_windows,
                    delivery: ObservationDelivery::Full,
                })
                .await
                .map(Some)
        } else {
            Ok(None)
        }
    }

    async fn external_observe_after(
        &self,
        after: Option<&ObserveAfter>,
        baseline: Option<Observation>,
    ) -> Result<(Option<Observation>, bool), ControllerError> {
        let Some(after) = after else {
            return Ok((None, true));
        };
        if after.timeout_ms == 0 || after.timeout_ms > MAX_TIMEOUT_MS {
            return Err(ControllerError::new(
                ErrorCode::InvalidInput,
                format!("observe_after.timeout_ms must be between 1 and {MAX_TIMEOUT_MS}"),
            ));
        }
        if after.quiet_ms > after.timeout_ms {
            return Err(ControllerError::new(
                ErrorCode::InvalidInput,
                "observe_after.quiet_ms cannot exceed timeout_ms",
            ));
        }
        let deadline = Instant::now() + Duration::from_millis(after.timeout_ms);
        let mut settled = true;
        if after.require_change {
            let wait = self
                .controller
                .wait_for(WaitRequest {
                    condition: WaitCondition::Change {
                        since_frame_id: baseline.as_ref().map(|base| base.metadata.frame_id),
                        target: after.target.clone(),
                    },
                    timeout_ms: remaining_timeout_ms(deadline).unwrap_or(1),
                    observe: false,
                })
                .await;
            settled = match wait {
                Ok(_) => true,
                Err(error) if error.code == ErrorCode::Timeout => false,
                Err(error) => return Err(error),
            };
        }
        if settled {
            if let Some(timeout_ms) = remaining_timeout_ms(deadline) {
                let wait = self
                    .controller
                    .wait_for(WaitRequest {
                        condition: WaitCondition::Idle {
                            quiet_ms: after.quiet_ms,
                            target: after.target.clone(),
                        },
                        timeout_ms,
                        observe: false,
                    })
                    .await;
                settled = match wait {
                    Ok(_) => true,
                    Err(error) if error.code == ErrorCode::Timeout => false,
                    Err(error) => return Err(error),
                };
            } else {
                settled = false;
            }
        }
        let delivery = match (after.delivery.clone(), baseline.as_ref()) {
            (AfterDelivery::Delta, Some(base)) => ObservationDelivery::Delta {
                since_frame_id: base.metadata.frame_id,
            },
            _ => ObservationDelivery::Full,
        };
        let observation = self
            .controller
            .observe(ObserveRequest {
                target: after.target.clone(),
                include_windows: after.include_windows,
                delivery,
            })
            .await?;
        Ok((Some(observation), settled))
    }

    async fn wait_result(
        &self,
        observe: bool,
        window: Option<WindowInfo>,
        element: Option<AccessibleNode>,
    ) -> Result<SessionWaitResult, ControllerError> {
        let observation = if observe {
            Some(self.controller.observe(ObserveRequest::default()).await?)
        } else {
            None
        };
        Ok(SessionWaitResult {
            matched: true,
            window,
            element,
            observation,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AccessibilityActionOutput {
    #[serde(flatten)]
    pub action: AccessibilityActionResult,
    pub settled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation: Option<Observation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchRequest {
    pub steps: Vec<BatchStep>,
    #[serde(default)]
    pub guard: StateGuard,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observe_after: Option<ObserveAfter>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
pub enum BatchStep {
    FocusWindow { request: FocusWindowRequest },
    MovePointer { request: MovePointerRequest },
    Click { request: ClickRequest },
    Drag { request: DragRequest },
    Scroll { request: ScrollRequest },
    Key { request: KeyRequest },
    TypeText { request: TypeTextRequest },
    WindowAction { request: WindowActionRequest },
    AccessibilityAction { request: AccessibilityActionRequest },
    LaunchApp { request: LaunchAppRequest },
    WaitFor { request: SessionWaitRequest },
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BatchStepResult {
    pub index: usize,
    pub value: Value,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BatchResult {
    pub ok: bool,
    pub steps: Vec<BatchStepResult>,
    pub settled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation: Option<Observation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionWaitRequest {
    pub condition: SessionWaitCondition,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_true")]
    pub observe: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "condition", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionWaitCondition {
    FrameChanged {
        #[serde(default)]
        since_frame_id: Option<u64>,
        #[serde(default)]
        target: ObserveTarget,
    },
    FrameIdle {
        #[serde(default = "default_quiet_ms")]
        quiet_ms: u64,
        #[serde(default)]
        target: ObserveTarget,
    },
    WindowMatched {
        selector: WindowSelector,
    },
    WindowState {
        window_ref: String,
        #[serde(default)]
        mapped: Option<bool>,
        #[serde(default)]
        active: Option<bool>,
        #[serde(default)]
        title_contains: Option<String>,
    },
    WindowClosed {
        window_ref: String,
    },
    ElementMatched {
        selector: ElementSelector,
    },
    ElementState {
        element_ref: String,
        #[serde(default)]
        states_all: Vec<String>,
        #[serde(default)]
        name_contains: Option<String>,
        #[serde(default)]
        text_contains: Option<String>,
        #[serde(default)]
        value: Option<ValueRange>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ValueRange {
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
}

impl ValueRange {
    fn matches(&self, value: Option<&atspi_controller::AccessibleValue>) -> bool {
        value.is_some_and(|value| {
            self.minimum.is_none_or(|minimum| value.current >= minimum)
                && self.maximum.is_none_or(|maximum| value.current <= maximum)
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SessionWaitResult {
    pub matched: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub element: Option<AccessibleNode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation: Option<Observation>,
}

impl SessionWaitResult {
    fn x11(result: x11_controller::WaitResult) -> Self {
        Self {
            matched: result.matched,
            window: result.window,
            element: None,
            observation: result.observation,
        }
    }

    fn observation(observation: Option<Observation>) -> Self {
        Self {
            matched: true,
            window: None,
            element: None,
            observation,
        }
    }
}

struct BatchGuardScope {
    require_frame: bool,
    include_current_pointer: bool,
    positions: Vec<x11_controller::Position>,
    semantic: bool,
}

fn batch_guard_scope(steps: &[BatchStep]) -> BatchGuardScope {
    let mut scope = BatchGuardScope {
        require_frame: false,
        include_current_pointer: false,
        positions: Vec::new(),
        semantic: false,
    };
    for step in steps {
        match step {
            BatchStep::MovePointer { request }
                if matches!(
                    &request.position,
                    x11_controller::Position::Window { .. }
                        | x11_controller::Position::WindowRelative { .. }
                ) =>
            {
                scope.require_frame = true;
                scope.positions.push(request.position.clone());
            }
            BatchStep::Click { request } => {
                scope.require_frame = true;
                if let Some(position) = &request.position {
                    scope.positions.push(position.clone());
                } else {
                    scope.include_current_pointer = true;
                }
            }
            BatchStep::Drag { request } => {
                scope.require_frame = true;
                scope.positions.push(request.from.clone());
                scope.positions.push(request.to.clone());
            }
            BatchStep::Scroll { request } => {
                if let Some(position) = &request.position {
                    scope.require_frame = true;
                    scope.positions.push(position.clone());
                }
            }
            BatchStep::AccessibilityAction { .. } => scope.semantic = true,
            _ => {}
        }
    }
    scope
}

fn batch_error(
    mut error: ControllerError,
    failed_step: usize,
    completed: &[BatchStepResult],
) -> ControllerError {
    error.details = Some(json!({
        "failed_step": failed_step,
        "completed_steps": completed,
        "cause": error.details,
    }));
    error
}

fn serde_value(value: impl Serialize) -> Result<Value, ControllerError> {
    serde_json::to_value(value).map_err(|error| {
        ControllerError::new(
            ErrorCode::Internal,
            format!("serialize batch step result: {error}"),
        )
    })
}

fn validate_timeout(timeout_ms: u64) -> Result<(), ControllerError> {
    if timeout_ms == 0 || timeout_ms > MAX_TIMEOUT_MS {
        Err(ControllerError::new(
            ErrorCode::InvalidInput,
            format!("timeout_ms must be between 1 and {MAX_TIMEOUT_MS}"),
        ))
    } else {
        Ok(())
    }
}

async fn wait_accessibility_tick(
    accessibility: &AccessibilityController,
    sequence: u64,
    deadline: Instant,
) -> Result<(), ControllerError> {
    if Instant::now() >= deadline {
        return Err(
            ControllerError::new(ErrorCode::Timeout, "wait condition timed out").retryable(true),
        );
    }
    if accessibility.event_driven() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if accessibility.wait_for_event(sequence, remaining).await {
            Ok(())
        } else {
            Err(
                ControllerError::new(ErrorCode::Timeout, "wait condition timed out")
                    .retryable(true),
            )
        }
    } else {
        wait_tick(deadline).await
    }
}

async fn wait_tick(deadline: Instant) -> Result<(), ControllerError> {
    if Instant::now() >= deadline {
        return Err(
            ControllerError::new(ErrorCode::Timeout, "wait condition timed out").retryable(true),
        );
    }
    sleep(WAIT_POLL.min(deadline.saturating_duration_since(Instant::now()))).await;
    Ok(())
}

fn batch_timeout_error() -> ControllerError {
    ControllerError::new(ErrorCode::Timeout, "batch deadline exceeded").retryable(true)
}

fn remaining_timeout_ms(deadline: Instant) -> Option<u64> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return None;
    }
    Some(
        u64::try_from(remaining.as_millis())
            .unwrap_or(u64::MAX)
            .clamp(1, MAX_TIMEOUT_MS),
    )
}

const fn default_timeout_ms() -> u64 {
    3_000
}

const fn default_quiet_ms() -> u64 {
    150
}

const fn default_true() -> bool {
    true
}

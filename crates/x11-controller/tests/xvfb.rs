use std::sync::{Arc, atomic::AtomicBool};

use x11_controller::{
    ControllerConfig, DesktopController, ListWindowsRequest, MovePointerRequest, ObserveRequest,
    Position, WaitCondition, WaitRequest,
};

#[tokio::test]
async fn controls_isolated_x11_display() {
    if std::env::var_os("X11_MCP_RUN_X11_TESTS").is_none() {
        return;
    }
    let display = std::env::var("DISPLAY").expect("DISPLAY must be set by the test harness");
    let controller = x11_controller::connect(
        ControllerConfig {
            display,
            allow_window_classes: Vec::new(),
            max_input_events_per_second: 200,
            paste_chord: vec!["CTRL".into(), "V".into()],
        },
        Arc::new(AtomicBool::new(false)),
    )
    .expect("connect to Xvfb");

    let capabilities = controller.capabilities().await.expect("capabilities");
    assert!(capabilities.extensions["xtest"]);
    assert_eq!(capabilities.screen.width, 800);
    assert_eq!(capabilities.screen.height, 600);

    let observation = controller
        .observe(ObserveRequest::default())
        .await
        .expect("desktop observation");
    assert!(observation.png.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert_eq!(observation.metadata.bounds.width, 800);

    controller
        .move_pointer(MovePointerRequest {
            position: Position::Screen { x: 50, y: 60 },
            observe_after: None,
        })
        .await
        .expect("move pointer");

    let windows = controller
        .list_windows(ListWindowsRequest::default())
        .await
        .expect("window list");
    assert!(windows.desktop_generation >= 1);

    let idle = controller
        .wait_for(WaitRequest {
            condition: WaitCondition::Idle { quiet_ms: 50 },
            timeout_ms: 1_000,
            observe: true,
        })
        .await
        .expect("desktop becomes idle");
    assert!(idle.matched);
    assert!(idle.observation.is_some());
}

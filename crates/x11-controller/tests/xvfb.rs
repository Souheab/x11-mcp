use std::sync::{Arc, atomic::AtomicBool};

use x11_controller::{
    ClickRequest, ControllerConfig, DesktopController, ErrorCode, ListWindowsRequest,
    MovePointerRequest, ObservationDelivery, ObserveAfter, ObserveRequest, ObserveTarget, Position,
    StateGuard, WaitCondition, WaitRequest,
};
use x11rb::{
    connection::Connection as _,
    protocol::xproto::{ConnectionExt as _, CreateGCAux, Rectangle},
};

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn controls_isolated_x11_display() {
    if std::env::var_os("X11_MCP_RUN_X11_TESTS").is_none() {
        return;
    }
    let display = std::env::var("DISPLAY").expect("DISPLAY must be set by the test harness");
    let controller = x11_controller::connect(
        ControllerConfig {
            display: display.clone(),
            allow_window_classes: Vec::new(),
            max_input_events_per_second: 200,
            paste_chord: vec!["CTRL".into(), "V".into()],
        },
        Arc::new(AtomicBool::new(false)),
    )
    .expect("connect to Xvfb");

    let capabilities = controller.capabilities().await.expect("capabilities");
    assert!(capabilities.extensions["xtest"]);
    assert!(capabilities.extensions["damage"]);
    assert_eq!(capabilities.screen.width, 800);
    assert_eq!(capabilities.screen.height, 600);

    let captures_before_idle = controller.capture_count().await.expect("capture count");
    controller
        .wait_for(WaitRequest {
            condition: WaitCondition::Idle {
                quiet_ms: 50,
                target: ObserveTarget::Desktop,
            },
            timeout_ms: 1_000,
            observe: false,
        })
        .await
        .expect("initial desktop idle");
    assert_eq!(
        controller.capture_count().await.expect("capture count"),
        captures_before_idle,
        "XDamage idle waits must not poll screenshots"
    );
    let observation = controller
        .observe(ObserveRequest::default())
        .await
        .expect("desktop observation");
    assert!(observation.images[0].starts_with(b"\x89PNG\r\n\x1a\n"));
    assert_eq!(observation.metadata.bounds.width, 800);

    let empty_delta = controller
        .observe(ObserveRequest {
            delivery: ObservationDelivery::Delta {
                since_frame_id: observation.metadata.frame_id,
            },
            ..ObserveRequest::default()
        })
        .await
        .expect("empty desktop delta");
    assert!(empty_delta.patches.is_empty());
    assert!(empty_delta.images.is_empty());

    let unguarded_click = controller
        .click(ClickRequest::default())
        .await
        .expect_err("clicks require a frame guard");
    assert_eq!(unguarded_click.code, ErrorCode::PreconditionFailed);
    assert!(unguarded_click.retryable);

    paint_root_rectangle(&display);
    let stale_click = controller
        .click(ClickRequest {
            guard: StateGuard {
                frame_id: Some(observation.metadata.frame_id),
                ..StateGuard::default()
            },
            ..ClickRequest::default()
        })
        .await
        .expect_err("intervening damage invalidates the guard");
    assert_eq!(stale_click.code, ErrorCode::PreconditionFailed);

    let changed_delta = controller
        .observe(ObserveRequest {
            delivery: ObservationDelivery::Delta {
                since_frame_id: observation.metadata.frame_id,
            },
            ..ObserveRequest::default()
        })
        .await
        .expect("changed desktop delta");
    assert!(!changed_delta.patches.is_empty());
    assert_eq!(changed_delta.patches.len(), changed_delta.images.len());
    for (index, patch) in changed_delta.patches.iter().enumerate() {
        assert_eq!(patch.image_index, index);
        assert!(changed_delta.images[index].starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    let target_mismatch = controller
        .observe(ObserveRequest {
            target: ObserveTarget::Region {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
            include_windows: false,
            delivery: ObservationDelivery::Delta {
                since_frame_id: changed_delta.metadata.frame_id,
            },
        })
        .await
        .expect_err("delta targets must match their base");
    assert_eq!(target_mismatch.code, ErrorCode::StaleFrame);

    let fresh = controller
        .observe(ObserveRequest::default())
        .await
        .expect("fresh guarded frame");
    let captures_before_settle = controller.capture_count().await.expect("capture count");
    controller
        .click(ClickRequest {
            guard: StateGuard {
                frame_id: Some(fresh.metadata.frame_id),
                ..StateGuard::default()
            },
            observe_after: Some(ObserveAfter {
                quiet_ms: 50,
                timeout_ms: 1_000,
                ..ObserveAfter::default()
            }),
            ..ClickRequest::default()
        })
        .await
        .expect("guarded click");
    let captures_after_settle = controller.capture_count().await.expect("capture count");
    assert_eq!(
        captures_after_settle.saturating_sub(captures_before_settle),
        1,
        "XDamage settling should capture only the requested final observation"
    );

    controller
        .move_pointer(MovePointerRequest {
            position: Position::Screen { x: 50, y: 60 },
            guard: StateGuard::default(),
            observe_after: None,
        })
        .await
        .expect("screen-absolute pointer movement remains unguarded");

    let windows = controller
        .list_windows(ListWindowsRequest::default())
        .await
        .expect("window list");
    assert!(windows.desktop_generation >= 1);

    let idle = controller
        .wait_for(WaitRequest {
            condition: WaitCondition::Idle {
                quiet_ms: 50,
                target: x11_controller::ObserveTarget::Desktop,
            },
            timeout_ms: 1_000,
            observe: true,
        })
        .await
        .expect("desktop becomes idle");
    assert!(idle.matched);
    assert!(idle.observation.is_some());

    let expired_base = controller
        .observe(ObserveRequest::default())
        .await
        .expect("history base")
        .metadata
        .frame_id;
    for _ in 0..65 {
        controller
            .observe(ObserveRequest::default())
            .await
            .expect("advance frame history");
    }
    let expired = controller
        .observe(ObserveRequest {
            delivery: ObservationDelivery::Delta {
                since_frame_id: expired_base,
            },
            ..ObserveRequest::default()
        })
        .await
        .expect_err("expired frame history");
    assert_eq!(expired.code, ErrorCode::StaleFrame);
}

fn paint_root_rectangle(display: &str) {
    let (connection, screen_index) = x11rb::connect(Some(display)).expect("second X11 connection");
    let screen = &connection.setup().roots[screen_index];
    let gc = connection.generate_id().expect("graphics context id");
    connection
        .create_gc(gc, screen.root, &CreateGCAux::new().foreground(0x00ff_00ff))
        .expect("create graphics context");
    connection
        .poly_fill_rectangle(
            screen.root,
            gc,
            &[Rectangle {
                x: 10,
                y: 10,
                width: 30,
                height: 20,
            }],
        )
        .expect("paint root rectangle");
    connection.free_gc(gc).expect("free graphics context");
    connection.flush().expect("flush root damage");
    connection
        .get_input_focus()
        .expect("root paint round-trip request")
        .reply()
        .expect("root paint round-trip reply");
}

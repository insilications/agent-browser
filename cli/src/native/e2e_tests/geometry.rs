//! Independently specified layout and painted-pixel checks for get box.
use super::*;
use crate::native::element::{main_world_execution_context, FrameContext};
use std::{future::Future, pin::Pin};

pub(super) const BUTTON: &str = "#inside-b-button";
pub(super) const BUTTON_STYLE: &str = "document.body.style.cssText='margin:0;height:2000px'; document.querySelector('#inside-b-button').style.cssText='position:absolute;left:20px;top:30px;width:80px;height:40px;border:0;padding:0;border-radius:0;background:rgb(231,17,83);color:rgb(231,17,83);box-sizing:border-box'";

// Box the command pipeline, whose debug future is too large for test stacks.
pub(super) fn command(
    state: &mut DaemonState,
    value: Value,
) -> Pin<Box<dyn Future<Output = Value> + '_>> {
    Box::pin(async move {
        let response = Box::pin(execute_command(&value, state)).await;
        assert_success(&response);
        response
    })
}

pub(super) async fn evaluate(
    state: &DaemonState,
    frame: Option<&FrameContext>,
    script: &str,
) -> Value {
    let browser = state.browser.as_ref().unwrap();
    let context =
        main_world_execution_context(&browser.client, browser.active_session_id().unwrap(), frame)
            .unwrap();
    let result = super::super::element::evaluate_in_context(
        &browser.client,
        &context,
        &format!("(()=>{{return eval({});}})()", json!(script)),
        true,
        false,
    )
    .await
    .unwrap();
    assert!(
        result.exception_details.is_none(),
        "evaluation failed: {script}: {:?}",
        result.exception_details
    );
    result.result.value.unwrap_or(Value::Null)
}

pub(super) async fn scene(state: &mut DaemonState, port: u16, host: &str) -> FrameContext {
    command(
        state,
        json!({"action":"navigate","url":format!("http://{host}:{port}/top")}),
    )
    .await;
    command(
        state,
        json!({"action":"viewport","width":1280,"height":900,"deviceScaleFactor":1}),
    )
    .await;
    evaluate(state, None, "document.body.style.cssText='margin:0;height:2000px'; document.querySelector('#outer').style.cssText='position:absolute;left:240px;top:160px;width:500px;height:400px;border:10px solid black;padding:0;transform-origin:0 0'").await;
    command(state, json!({"action":"frame","selector":"#outer"})).await;
    let outer = state.active_frame.clone().unwrap();
    evaluate(state, Some(&outer), "document.body.style.cssText='margin:0;height:2000px'; document.querySelector('#inner').style.cssText='position:absolute;left:30px;top:40px;width:350px;height:250px;border:5px solid black;padding:0;transform-origin:0 0'").await;
    command(state, json!({"action":"frame","selector":"#inner"})).await;
    evaluate(state, state.active_frame.as_ref(), BUTTON_STYLE).await;
    outer
}

async fn expect_box(
    state: &mut DaemonState,
    selector: &str,
    relative: Option<&str>,
    expected: [f64; 4],
) -> Value {
    let mut request = json!({"action":"boundingbox","selector":selector});
    if let Some(relative) = relative {
        request["relativeTo"] = json!(relative);
    }
    let response = command(state, request).await;
    let actual = &response["data"];
    assert!(
        actual
            .as_object()
            .unwrap()
            .keys()
            .all(|key| ["x", "y", "width", "height", "lifecycle"].contains(&key.as_str())),
        "{response}"
    );
    for (key, expected) in ["x", "y", "width", "height"].into_iter().zip(expected) {
        let actual = actual[key].as_f64().unwrap();
        assert!(
            (actual - expected).abs() < 0.02,
            "{key}: expected {expected}, got {actual}; {response}"
        );
    }
    response
}

pub(super) async fn button_ref(state: &mut DaemonState) -> String {
    let snapshot = command(state, json!({"action":"snapshot","interactive":true})).await;
    snapshot["data"]["refs"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, entry)| entry["name"] == "Click inside B")
        .unwrap()
        .0
        .clone()
}

#[tokio::test]
#[ignore]
async fn e2e_box_top_viewport_layout_refs_scroll_and_pixels() {
    let (_env, dir) = binding_test_env();
    let (port, server) = start_a11y_frame_server().await;
    let mut state = DaemonState::new();
    command(
        &mut state,
        json!({"action":"launch","headless":true,"args":["--site-per-process"]}),
    )
    .await;
    for host in ["127.0.0.1", "localhost"] {
        let outer = scene(&mut state, port, host).await;
        let inner = state.active_frame.clone().unwrap();
        let top_sid = state.browser.as_ref().unwrap().active_session_id().unwrap();
        assert_eq!(outer.session_id == top_sid, host == "127.0.0.1");
        assert_eq!(outer.session_id, inner.session_id);
        expect_box(&mut state, BUTTON, None, [20., 30., 80., 40.]).await;
        expect_box(
            &mut state,
            BUTTON,
            Some("frame-viewport"),
            [20., 30., 80., 40.],
        )
        .await;
        expect_box(
            &mut state,
            BUTTON,
            Some("top-viewport"),
            [305., 245., 80., 40.],
        )
        .await;
        let reference = button_ref(&mut state).await;
        expect_box(
            &mut state,
            &format!("@{reference}"),
            Some("top-viewport"),
            [305., 245., 80., 40.],
        )
        .await;
        assert_eq!(
            state.active_frame.as_ref().unwrap().frame_id,
            inner.frame_id
        );
        command(&mut state, json!({"action":"mainframe"})).await;
        expect_box(
            &mut state,
            &reference,
            Some("top-viewport"),
            [305., 245., 80., 40.],
        )
        .await;
        assert!(state.active_frame.is_none());
        // The same border box in the main document agrees in both modes.
        expect_box(&mut state, "#outer", None, [240., 160., 520., 420.]).await;
        expect_box(
            &mut state,
            "#outer",
            Some("top-viewport"),
            [240., 160., 520., 420.],
        )
        .await;

        if host == "localhost" {
            let path = dir.path().join("box.png");
            command(&mut state, json!({"action":"screenshot","path":path})).await;
            let pixels = image::open(path).unwrap().to_rgb8();
            let painted: Vec<_> = pixels
                .enumerate_pixels()
                .filter(|(_, _, p)| p.0 == [231, 17, 83])
                .map(|(x, y, _)| (x, y))
                .collect();
            assert!(!painted.is_empty());
            let bounds = [
                painted.iter().map(|p| p.0).min().unwrap(),
                painted.iter().map(|p| p.1).min().unwrap(),
                painted.iter().map(|p| p.0).max().unwrap() + 1,
                painted.iter().map(|p| p.1).max().unwrap() + 1,
            ];
            for (actual, expected) in bounds.into_iter().zip([305, 245, 385, 285]) {
                assert!(actual.abs_diff(expected) <= 1, "painted bounds: {bounds:?}");
            }
        }

        // Fractional content-box sizing, asymmetric padding, nonuniform scale.
        evaluate(&state, None, "document.querySelector('#outer').style.cssText+=';width:500.5px;height:400.5px;padding:7px 13px 11px 17px;transform:scale(1.25,1.5)'").await;
        expect_box(
            &mut state,
            &reference,
            Some("top-viewport"),
            [342.5, 298., 100., 60.],
        )
        .await;
        evaluate(
            &state,
            None,
            "document.querySelector('#outer').style.boxSizing='border-box'",
        )
        .await;
        expect_box(
            &mut state,
            &reference,
            Some("top-viewport"),
            [342.5, 298., 100., 60.],
        )
        .await;
        // Scaling at both levels composes, without applying same-process offsets twice.
        evaluate(
            &state,
            Some(&outer),
            "document.querySelector('#inner').style.transform='scale(2,0.5)'",
        )
        .await;
        expect_box(
            &mut state,
            &reference,
            Some("top-viewport"),
            [373.75, 271.75, 200., 30.],
        )
        .await;

        evaluate(&state, None, "document.querySelector('#outer').style.cssText='position:absolute;left:240px;top:160px;width:500px;height:400px;border:10px solid black;padding:0'").await;
        evaluate(
            &state,
            Some(&outer),
            "document.querySelector('#inner').style.transform='none'; window.scrollTo(0,20)",
        )
        .await;
        evaluate(&state, Some(&inner), "window.scrollTo(0,10)").await;
        evaluate(&state, None, "window.scrollTo(0,100)").await;
        expect_box(
            &mut state,
            &reference,
            Some("top-viewport"),
            [305., 115., 80., 40.],
        )
        .await;
        assert_eq!(evaluate(&state, None, "scrollY").await, 100);
        assert_eq!(evaluate(&state, Some(&outer), "scrollY").await, 20);
        assert_eq!(evaluate(&state, Some(&inner), "scrollY").await, 10);
        evaluate(&state, None, "window.scrollTo(0,400)").await;
        expect_box(
            &mut state,
            &reference,
            Some("top-viewport"),
            [305., -185., 80., 40.],
        )
        .await;
        command(
            &mut state,
            json!({"action":"viewport","width":1280,"height":900,"deviceScaleFactor":2}),
        )
        .await;
        expect_box(
            &mut state,
            &reference,
            Some("top-viewport"),
            [305., -185., 80., 40.],
        )
        .await;
    }
    command(&mut state, json!({"action":"close"})).await;
    server.abort();
}

#[tokio::test]
#[ignore]
async fn e2e_box_top_viewport_fractional_border_box_padding() {
    let (_env, _dir) = binding_test_env();
    let (port, server) = start_a11y_frame_server().await;
    let mut state = DaemonState::new();
    command(
        &mut state,
        json!({"action":"launch","headless":true,"args":["--site-per-process"]}),
    )
    .await;
    for host in ["127.0.0.1", "localhost"] {
        let outer = scene(&mut state, port, host).await;
        let top_sid = state.browser.as_ref().unwrap().active_session_id().unwrap();
        assert_eq!(outer.session_id == top_sid, host == "127.0.0.1");
        let reference = button_ref(&mut state).await;
        // Used padding is fractional, but differs from computed CSS padding.
        // The second case leaves only 1/32 CSS px of content on each axis.
        for (style, inset_x, inset_y) in [
            (
                "width:100px;height:100px;border:0;padding:49.499px",
                49.484375,
                49.484375,
            ),
            (
                "width:1px;height:1px;border:0;padding:0.499px",
                0.484375,
                0.484375,
            ),
            (
                "width:100.5px;height:80.5px;border:solid;border-width:1px 2px 3px 4px;padding:37.999px 46.499px",
                50.484375,
                38.984375,
            ),
            // Percentage padding is already layout-rounded in computed style;
            // truncating its serialized value again would also invent scale.
            (
                "width:98.0625px;height:98.0625px;border:0;padding:3.829345703125%",
                49.015625,
                49.015625,
            ),
        ] {
            for (sx, sy) in [(1., 1.), (1.25, 1.5)] {
                evaluate(
                    &state,
                    None,
                    &format!("document.querySelector('#outer').style.cssText='position:absolute;left:240px;top:160px;box-sizing:border-box;transform-origin:0 0;transform:scale({sx},{sy});{style}'"),
                )
                .await;
                // Inner iframe origin (35,45) plus button origin (20,30).
                let expected = [
                    240. + (inset_x + 55.) * sx,
                    160. + (inset_y + 75.) * sy,
                    80. * sx,
                    40. * sy,
                ];
                expect_box(&mut state, BUTTON, Some("top-viewport"), expected).await;
                expect_box(&mut state, &reference, Some("top-viewport"), expected).await;
            }
        }
        // A positive border box is not enough when padding consumes all content.
        evaluate(&state, None, "document.querySelector('#outer').style.cssText='width:100px;height:100px;box-sizing:border-box;border:0;padding:50px'").await;
        let response = Box::pin(execute_command(
            &json!({"action":"boundingbox","selector":BUTTON,"relativeTo":"top-viewport"}),
            &mut state,
        ))
        .await;
        assert_eq!(response["success"], false, "zero content size: {response}");
    }
    command(&mut state, json!({"action":"close"})).await;
    server.abort();
}

#[tokio::test]
#[ignore]
async fn e2e_box_top_viewport_renderer_replacement_and_removal() {
    let (_env, _dir) = binding_test_env();
    let (port, server) = start_a11y_frame_server().await;
    let mut state = DaemonState::new();
    command(
        &mut state,
        json!({"action":"launch","headless":true,"args":["--site-per-process"]}),
    )
    .await;
    let outer = scene(&mut state, port, "localhost").await;
    let original = state.active_frame.clone().unwrap();
    for (host, dedicated) in [("localhost", true), ("127.0.0.1", false)] {
        let stale = button_ref(&mut state).await;
        evaluate(
            &state,
            Some(&outer),
            &format!("document.querySelector('#inner').src='http://{host}:{port}/inner'; true"),
        )
        .await;
        wait_for_frame_destination(
            &mut state,
            &original.frame_id,
            &outer.session_id,
            host,
            dedicated,
        )
        .await;
        // No reselect: the existing preparation follows this browsing frame.
        command(
            &mut state,
            json!({"action":"evaluate","script":BUTTON_STYLE}),
        )
        .await;
        assert_eq!(
            state.active_frame.as_ref().unwrap().frame_id,
            original.frame_id
        );
        assert_eq!(
            state.active_frame.as_ref().unwrap().session_id != outer.session_id,
            dedicated
        );
        expect_box(
            &mut state,
            BUTTON,
            Some("top-viewport"),
            [305., 245., 80., 40.],
        )
        .await;
        let response = Box::pin(execute_command(
            &json!({"action":"boundingbox","selector":stale,"relativeTo":"top-viewport"}),
            &mut state,
        ))
        .await;
        assert_eq!(response["success"], false, "{response}");
        let fresh = button_ref(&mut state).await;
        expect_box(
            &mut state,
            &fresh,
            Some("top-viewport"),
            [305., 245., 80., 40.],
        )
        .await;
        // In the dedicated case this crosses two renderer boundaries.
        evaluate(
            &state,
            None,
            "document.querySelector('#outer').style.transform='scale(1.25,1.5)'",
        )
        .await;
        evaluate(
            &state,
            Some(&outer),
            "document.querySelector('#inner').style.transform='scale(2,0.5)'",
        )
        .await;
        expect_box(
            &mut state,
            &fresh,
            Some("top-viewport"),
            [352.5, 261.25, 200., 30.],
        )
        .await;
        evaluate(
            &state,
            None,
            "document.querySelector('#outer').style.transform='none'",
        )
        .await;
        evaluate(
            &state,
            Some(&outer),
            "document.querySelector('#inner').style.transform='none'",
        )
        .await;
    }
    evaluate(
        &state,
        Some(&outer),
        "document.querySelector('#inner').remove()",
    )
    .await;
    let response = Box::pin(execute_command(&json!({"action":"boundingbox","selector":BUTTON,"relativeTo":"top-viewport","timeout":1000}), &mut state)).await;
    assert_eq!(response["success"], false, "{response}");
    assert_eq!(response["code"], "frame_gone", "{response}");
    command(&mut state, json!({"action":"close"})).await;
    server.abort();
}

#[tokio::test]
#[ignore]
async fn e2e_box_top_viewport_rejects_unsupported_geometry() {
    let (_env, _dir) = binding_test_env();
    let (port, server) = start_a11y_frame_server().await;
    let mut state = DaemonState::new();
    command(
        &mut state,
        json!({"action":"launch","headless":true,"args":["--site-per-process"]}),
    )
    .await;
    // Guards must behave the same regardless of renderer placement.
    for host in ["127.0.0.1", "localhost"] {
        let outer = scene(&mut state, port, host).await;
        for style in [
            "transform:rotate(10deg)",
            "transform:skewX(10deg)",
            "transform:scaleX(-1)",
            "transform:scale(0)",
            "transform:perspective(500px) rotateY(10deg)",
            "rotate:10deg",
            "scale:-1 1",
            "perspective:500px",
            "zoom:1.2",
        ] {
            evaluate(
                &state,
                Some(&outer),
                &format!("document.querySelector('#inner').style.cssText+=';{style}'"),
            )
            .await;
            let response = Box::pin(execute_command(
                &json!({"action":"boundingbox","selector":BUTTON,"relativeTo":"top-viewport"}),
                &mut state,
            ))
            .await;
            assert_eq!(response["success"], false, "{style}: {response}");
            assert!(
                response["data"].is_null(),
                "must not silently return local coordinates: {response}"
            );
            command(
                &mut state,
                json!({"action":"boundingbox","selector":BUTTON}),
            )
            .await;
            evaluate(&state, Some(&outer), "let e=document.querySelector('#inner'); for(let p of ['transform','rotate','scale','perspective','zoom']) e.style.removeProperty(p)").await;
        }
        evaluate(
            &state,
            Some(&outer),
            "document.querySelector('#inner').parentElement.style.transform='rotate(5deg)'",
        )
        .await;
        let response = Box::pin(execute_command(
            &json!({"action":"boundingbox","selector":BUTTON,"relativeTo":"top-viewport"}),
            &mut state,
        ))
        .await;
        assert_eq!(response["success"], false, "ancestor rotation: {response}");
        evaluate(
            &state,
            Some(&outer),
            "document.querySelector('#inner').parentElement.style.transform='none'",
        )
        .await;
        evaluate(
            &state,
            state.active_frame.as_ref(),
            "document.querySelector('#inside-b-button').style.display='none'",
        )
        .await;
        let response = Box::pin(execute_command(
            &json!({"action":"boundingbox","selector":BUTTON,"relativeTo":"top-viewport"}),
            &mut state,
        ))
        .await;
        assert_eq!(response["success"], false, "no layout: {response}");
        evaluate(
            &state,
            state.active_frame.as_ref(),
            "document.querySelector('#inside-b-button').style.display='block'",
        )
        .await;
        let browser = state.browser.as_ref().unwrap();
        browser
            .client
            .send_command(
                "Emulation.setPageScaleFactor",
                Some(json!({"pageScaleFactor":1.5})),
                Some(browser.active_session_id().unwrap()),
            )
            .await
            .unwrap();
        let response = Box::pin(execute_command(
            &json!({"action":"boundingbox","selector":BUTTON,"relativeTo":"top-viewport"}),
            &mut state,
        ))
        .await;
        assert_eq!(response["success"], false, "pinch zoom: {response}");
        let browser = state.browser.as_ref().unwrap();
        browser
            .client
            .send_command(
                "Emulation.setPageScaleFactor",
                Some(json!({"pageScaleFactor":1})),
                Some(browser.active_session_id().unwrap()),
            )
            .await
            .unwrap();
        expect_box(
            &mut state,
            BUTTON,
            Some("top-viewport"),
            [305., 245., 80., 40.],
        )
        .await;
    }
    command(&mut state, json!({"action":"close"})).await;
    server.abort();
}

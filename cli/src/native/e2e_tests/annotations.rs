//! Annotation integration tests use independent fixture coordinates and real
//! painted pixels, not expected values computed by the projection helper.
use super::geometry::{command, evaluate, scene};
use super::*;

fn annotation<'a>(response: &'a Value, name: &str) -> &'a Value {
    response["data"]["annotations"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["name"] == name))
        .unwrap_or_else(|| panic!("Missing annotation {name}: {response}"))
}

fn assert_box(actual: &Value, expected: [f64; 4]) {
    for (key, expected) in ["x", "y", "width", "height"].into_iter().zip(expected) {
        assert!(
            (actual[key].as_f64().unwrap() - expected).abs() < 0.02,
            "{key}: expected {expected}; {actual}"
        );
    }
}

async fn nested_annotation(host: &str) {
    let (_env, dir) = binding_test_env();
    let (port, server) = start_a11y_frame_server().await;
    let mut state = DaemonState::new();
    command(
        &mut state,
        json!({"action":"launch","headless":true,"args":["--site-per-process"]}),
    )
    .await;
    let outer = scene(&mut state, port, host).await;
    let selected = state.active_frame.clone().unwrap();
    let page_session = state.browser.as_ref().unwrap().active_session_id().unwrap();
    assert_eq!(outer.session_id == page_session, host == "127.0.0.1");
    assert_eq!(selected.session_id, outer.session_id);
    let response = command(
        &mut state,
        json!({"action":"screenshot","annotate":true,"path":dir.path().join("nested.png")}),
    )
    .await;
    assert_eq!(state.active_frame.as_ref(), Some(&selected));
    let cropped_path = dir.path().join("nested-crop.png");
    let cropped = command(
        &mut state,
        json!({"action":"screenshot","annotate":true,
        "selector":super::geometry::BUTTON,"path":cropped_path}),
    )
    .await;
    assert_box(
        &annotation(&cropped, "Click inside B")["box"],
        [0., 0., 80., 40.],
    );
    let pixels = image::open(cropped_path).unwrap().to_rgb8();
    assert_eq!(pixels.dimensions(), (80, 40));
    assert_eq!(pixels.get_pixel(40, 20).0, [231, 17, 83]);
    assert_eq!(state.active_frame.as_ref(), Some(&selected));
    command(&mut state, json!({"action":"close"})).await;
    server.abort();
    assert_box(
        &annotation(&response, "Click inside B")["box"],
        [305., 245., 80., 40.],
    );
}

#[tokio::test]
#[ignore]
async fn e2e_screenshot_annotations_same_process() {
    nested_annotation("127.0.0.1").await;
}

#[tokio::test]
#[ignore]
async fn e2e_screenshot_annotations_mixed_renderers() {
    nested_annotation("localhost").await;
}

#[tokio::test]
#[ignore]
async fn e2e_screenshot_annotations_scrolled_crop_and_full() {
    let (_env, dir) = binding_test_env();
    let mut state = DaemonState::new();
    command(&mut state, json!({"action":"launch","headless":true})).await;
    command(&mut state, json!({"action":"setcontent","html":"<style>body{margin:0;height:2000px}button{position:absolute;left:120px;top:500px;width:80px;height:40px;border:0;padding:0;border-radius:0;background:rgb(231,17,83);color:rgb(231,17,83)}</style><button id='probe'>Probe</button>"})).await;
    command(
        &mut state,
        json!({"action":"viewport","width":800,"height":600,"deviceScaleFactor":1}),
    )
    .await;
    evaluate(&state, None, "window.scrollTo(0,200)").await;
    let path = dir.path().join("crop.png");
    let cropped = command(
        &mut state,
        json!({"action":"screenshot","annotate":true,"selector":"#probe","path":path}),
    )
    .await;
    let full = command(&mut state, json!({"action":"screenshot","annotate":true,"selector":"#probe","fullPage":true,"path":dir.path().join("full.png")})).await;
    let scroll = evaluate(&state, None, "scrollY").await;
    command(&mut state, json!({"action":"close"})).await;
    assert_eq!(scroll, 200);
    assert_box(&annotation(&cropped, "Probe")["box"], [0., 0., 80., 40.]);
    let pixels = image::open(path).unwrap().to_rgb8();
    assert_eq!(pixels.dimensions(), (80, 40));
    assert_eq!(
        pixels.get_pixel(40, 20).0,
        [231, 17, 83],
        "crop must contain the painted target"
    );
    assert_box(&annotation(&full, "Probe")["box"], [120., 500., 80., 40.]);
}

#[tokio::test]
#[ignore]
async fn e2e_screenshot_annotations_scaling_clipping_padding_and_dpr() {
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
        for sizing in ["content-box", "border-box"] {
            evaluate(&state, None, &format!("document.querySelector('#outer').style.cssText+=';width:500.5px;height:400.5px;padding:7px 13px 11px 17px;transform:scale(1.25,1.5);box-sizing:{sizing}'")).await;
            let response = command(
                &mut state,
                json!({"action":"screenshot","annotate":true,"path":dir.path().join("scaled.png")}),
            )
            .await;
            assert_box(
                &annotation(&response, "Click inside B")["box"],
                [342.5, 298., 100., 60.],
            );
        }
        // Tiny positive content must not invent scale or fail. The target is
        // outside the actual content clip, even though get-box remains 80x40.
        for style in [
            "width:100px;height:100px;border:0;padding:49.499px",
            "width:1px;height:1px;border:0;padding:0.499px",
            "width:100.5px;height:80.5px;border:solid;border-width:1px 2px 3px 4px;padding:37.999px 46.499px",
            "width:98.0625px;height:98.0625px;border:0;padding:3.829345703125%",
        ] {
            for transform in ["none", "scale(1.25,1.5)"] {
                evaluate(&state, None, &format!("document.querySelector('#outer').style.cssText='position:absolute;left:240px;top:160px;box-sizing:border-box;transform-origin:0 0;transform:{transform};{style}'")).await;
                let response = command(&mut state, json!({"action":"screenshot","annotate":true,"path":dir.path().join("tiny.png")})).await;
                assert!(response["data"]["annotations"].as_array().is_none_or(Vec::is_empty), "{response}");
            }
        }
        evaluate(&state, None, "document.querySelector('#outer').style.cssText='position:absolute;left:240px;top:160px;width:500px;height:400px;border:10px solid black;padding:0'").await;
        evaluate(
            &state,
            Some(&outer),
            "document.querySelector('#inner').style.width='40px'",
        )
        .await;
        for dpr in [1, 2] {
            command(
                &mut state,
                json!({"action":"viewport","width":1280,"height":900,"deviceScaleFactor":dpr}),
            )
            .await;
            let path = dir.path().join("partial.png");
            let response = command(
                &mut state,
                json!({"action":"screenshot","annotate":true,"path":path}),
            )
            .await;
            assert_box(
                &annotation(&response, "Click inside B")["box"],
                [305., 245., 80., 40.],
            );
            let image = image::open(path).unwrap().to_rgb8();
            assert_eq!(image.dimensions(), (1280 * dpr, 900 * dpr));
            assert_eq!(image.get_pixel(315 * dpr, 270 * dpr).0, [231, 17, 83]);
            let border = image.get_pixel(305 * dpr, 270 * dpr).0;
            assert!(
                border[0] > 240 && border[1] < 10 && border[2] < 30,
                "outline: {border:?}"
            );
            assert_eq!(
                image.get_pixel(326 * dpr, 270 * dpr).0,
                [0, 0, 0],
                "outline must stop at iframe content, not paint over border"
            );
        }
        command(
            &mut state,
            json!({"action":"viewport","width":1280,"height":900,"deviceScaleFactor":1}),
        )
        .await;
        evaluate(
            &state,
            Some(&outer),
            "document.querySelector('#inner').style.width='350px';window.scrollTo(0,20)",
        )
        .await;
        evaluate(&state, Some(&inner), "window.scrollTo(0,10)").await;
        evaluate(&state, None, "window.scrollTo(0,100)").await;
        let response = command(
            &mut state,
            json!({"action":"screenshot","annotate":true,"path":dir.path().join("scroll.png")}),
        )
        .await;
        assert_box(
            &annotation(&response, "Click inside B")["box"],
            [305., 115., 80., 40.],
        );
        let full=command(&mut state,json!({"action":"screenshot","annotate":true,"fullPage":true,"path":dir.path().join("scroll-full.png")})).await;
        assert_box(
            &annotation(&full, "Click inside B")["box"],
            [305., 215., 80., 40.],
        );
        assert_eq!(evaluate(&state, None, "scrollY").await, 100);
        assert_eq!(evaluate(&state, Some(&outer), "scrollY").await, 20);
        assert_eq!(evaluate(&state, Some(&inner), "scrollY").await, 10);
        assert_eq!(state.active_frame.as_ref(), Some(&inner));
    }
    command(&mut state, json!({"action":"close"})).await;
    server.abort();
}

#[tokio::test]
#[ignore]
async fn e2e_screenshot_annotations_crop_ref_provenance_and_renderer_replacement() {
    let (_env, dir) = binding_test_env();
    let (port, server) = start_a11y_frame_server().await;
    let mut state = DaemonState::new();
    command(
        &mut state,
        json!({"action":"launch","headless":true,"args":["--site-per-process"]}),
    )
    .await;
    let outer = scene(&mut state, port, "localhost").await;
    let inner = state.active_frame.clone().unwrap();
    for prefixed in [false, true] {
        let reference = super::geometry::button_ref(&mut state).await;
        let selector = if prefixed {
            format!("@{reference}")
        } else {
            reference
        };
        // Main snapshot does not recursively discover the grandchild. The crop
        // still belongs to that grandchild, not to the refreshed main ref map.
        command(&mut state, json!({"action":"mainframe"})).await;
        let path = dir.path().join("foreign.png");
        let response = command(
            &mut state,
            json!({"action":"screenshot","annotate":true,"selector":selector,"path":path}),
        )
        .await;
        assert!(state.active_frame.is_none());
        assert!(!response["data"]["annotations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "Click inside B"));
        let image = image::open(path).unwrap().to_rgb8();
        assert_eq!(image.dimensions(), (80, 40));
        assert_eq!(image.get_pixel(40, 20).0, [231, 17, 83]);
        command(&mut state, json!({"action":"frame","selector":"#outer"})).await;
        command(&mut state, json!({"action":"frame","selector":"#inner"})).await;
    }
    for (host, dedicated) in [("localhost", true), ("127.0.0.1", false)] {
        let stale = super::geometry::button_ref(&mut state).await;
        evaluate(
            &state,
            Some(&outer),
            &format!("document.querySelector('#inner').src='http://{host}:{port}/inner';true"),
        )
        .await;
        wait_for_frame_destination(
            &mut state,
            &inner.frame_id,
            &outer.session_id,
            host,
            dedicated,
        )
        .await;
        command(
            &mut state,
            json!({"action":"evaluate","script":super::geometry::BUTTON_STYLE}),
        )
        .await;
        assert_eq!(
            state.active_frame.as_ref().unwrap().session_id != outer.session_id,
            dedicated
        );
        let failure=Box::pin(execute_command(&json!({"action":"screenshot","annotate":true,"selector":stale,"path":dir.path().join("stale.png")}),&mut state)).await;
        assert_eq!(failure["success"], false, "{failure}");
        assert!(!dir.path().join("stale.png").exists());
        let response=command(&mut state,json!({"action":"screenshot","annotate":true,"path":dir.path().join("new-document.png")})).await;
        assert_box(
            &annotation(&response, "Click inside B")["box"],
            [305., 245., 80., 40.],
        );
        // The dedicated case crosses two renderer boundaries with two scales.
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
        let response = command(
            &mut state,
            json!({"action":"screenshot","annotate":true,"path":dir.path().join("two-scales.png")}),
        )
        .await;
        assert_box(
            &annotation(&response, "Click inside B")["box"],
            [352.5, 261.25, 200., 30.],
        );
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
    let response=Box::pin(execute_command(&json!({"action":"screenshot","annotate":true,"path":dir.path().join("gone.png"),"timeout":1000}),&mut state)).await;
    assert_eq!(response["code"], "frame_gone", "{response}");
    assert!(!dir.path().join("gone.png").exists());
    command(&mut state, json!({"action":"close"})).await;
    server.abort();
}

#[tokio::test]
#[ignore]
async fn e2e_screenshot_annotations_explicit_failures_leave_no_artifacts() {
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
            "width:100px;height:100px;box-sizing:border-box;border:0;padding:50px",
        ] {
            evaluate(&state,Some(&outer),&format!("document.querySelector('#inner').style.cssText='position:absolute;left:30px;top:40px;width:350px;height:250px;border:5px solid black;padding:0;{style}'")).await;
            let response=Box::pin(execute_command(&json!({"action":"screenshot","annotate":true,"ifChanged":true,"path":dir.path().join("failure.png")}),&mut state)).await;
            assert_eq!(response["success"], false, "{style}: {response}");
            assert!(!dir.path().join("failure.png").exists());
            assert_eq!(
                evaluate(
                    &state,
                    None,
                    "document.querySelectorAll('[id^=__agent_browser_annotations_]').length"
                )
                .await,
                0
            );
        }
    }
    command(&mut state, json!({"action":"close"})).await;
    server.abort();
}

#[tokio::test]
#[ignore]
async fn e2e_screenshot_annotations_fractional_overlay_isolation_and_conditionals() {
    let (_env, dir) = binding_test_env();
    let mut state = DaemonState::new();
    command(&mut state, json!({"action":"launch","headless":true})).await;
    command(
        &mut state,
        json!({"action":"viewport","width":800,"height":600,"deviceScaleFactor":1}),
    )
    .await;
    command(&mut state,json!({"action":"setcontent","html":"<style>html{transform:translate(0.25px,0.5px)}body{margin:0;height:1200px}div{all:unset!important;transform:scale(4)!important}button{position:absolute;left:120.125px;top:100.25px;width:80.5px;height:40.25px;border:0;padding:0;border-radius:0;background:rgb(231,17,83);color:rgb(231,17,83)}</style><div id='__agent_browser_annotations__'></div><button id='probe'>Probe</button>"})).await;
    let before=evaluate(&state,None,"[scrollX,scrollY,document.documentElement.scrollWidth,document.documentElement.scrollHeight]").await;
    let first=command(&mut state,json!({"action":"screenshot","annotate":true,"ifChanged":true,"path":dir.path().join("first.png")})).await;
    assert_box(
        &annotation(&first, "Probe")["box"],
        [120.375, 100.75, 80.5, 40.25],
    );
    let second=command(&mut state,json!({"action":"screenshot","annotate":true,"ifChanged":true,"path":dir.path().join("suppressed.png")})).await;
    assert_eq!(second["data"]["changed"], false, "{second}");
    assert_eq!(second["data"]["revision"], 2);
    assert!(second["data"]["path"].is_null());
    assert!(!dir.path().join("suppressed.png").exists());
    assert_eq!(
        annotation(&first, "Probe")["ref"],
        annotation(&second, "Probe")["ref"]
    );
    assert_eq!(evaluate(&state,None,"[scrollX,scrollY,document.documentElement.scrollWidth,document.documentElement.scrollHeight]").await,before);
    assert_eq!(
        evaluate(
            &state,
            None,
            "document.querySelectorAll('[id^=__agent_browser_annotations_]').length"
        )
        .await,
        1,
        "page-owned legacy ID must survive"
    );
    evaluate(
        &state,
        None,
        "document.documentElement.style.transform='scale(1.1)'",
    )
    .await;
    let failure=Box::pin(execute_command(&json!({"action":"screenshot","annotate":true,"ifChanged":true,"path":dir.path().join("unsupported.png")}),&mut state)).await;
    assert_eq!(failure["success"], false, "{failure}");
    assert!(!dir.path().join("unsupported.png").exists());
    evaluate(&state,None,"document.documentElement.style.transform='translate(0.25px,0.5px)'; document.querySelector('#probe').style.left='121.125px'").await;
    let third=command(&mut state,json!({"action":"screenshot","annotate":true,"ifChanged":true,"path":dir.path().join("moved.png")})).await;
    assert_eq!(third["data"]["changed"], true, "{third}");
    assert_eq!(
        third["data"]["revision"], 3,
        "geometry must not become a new baseline key"
    );
    let r = annotation(&third, "Probe")["ref"].as_str().unwrap();
    command(
        &mut state,
        json!({"action":"boundingbox","selector":format!("@{r}"),"relativeTo":"top-viewport"}),
    )
    .await;
    command(&mut state, json!({"action":"close"})).await;
}

#[tokio::test]
#[ignore]
async fn e2e_screenshot_annotations_offscreen_fixed_and_content_clipped_crops() {
    let (_env, dir) = binding_test_env();
    let mut state = DaemonState::new();
    command(&mut state, json!({"action":"launch","headless":true})).await;
    command(
        &mut state,
        json!({"action":"viewport","width":800,"height":600,"deviceScaleFactor":1}),
    )
    .await;
    command(&mut state,json!({"action":"setcontent","html":"<style>body{margin:0;height:2000px}button{position:absolute;width:80px;height:40px;border:0;padding:0;border-radius:0;background:rgb(231,17,83);color:rgb(231,17,83)}#fixed{position:fixed;left:120px;top:50px}#offscreen{left:240px;top:1000px}#clipped{left:-10.5px;top:300.25px;width:80.5px;height:40.25px}#hidden{display:none}</style><button id='fixed'>Fixed</button><button id='offscreen'>Offscreen</button><button id='clipped'>Clipped</button><button id='hidden'>Hidden</button>"})).await;
    evaluate(&state, None, "scrollTo(0,200)").await;
    let path = dir.path().join("fixed-full.png");
    let full = command(
        &mut state,
        json!({"action":"screenshot","annotate":true,"fullPage":true,"path":path}),
    )
    .await;
    assert_box(&annotation(&full, "Fixed")["box"], [120., 250., 80., 40.]);
    let image = image::open(path).unwrap().to_rgb8();
    assert_eq!(
        image.get_pixel(160, 270).0,
        [231, 17, 83],
        "full capture must preserve fixed-element placement relative to measured scroll"
    );
    assert_eq!(
        image.dimensions(),
        (800, 2000),
        "overlay labels must not enlarge the capture"
    );
    let path = dir.path().join("offscreen.png");
    let crop = command(
        &mut state,
        json!({"action":"screenshot","annotate":true,"selector":"#offscreen","path":path}),
    )
    .await;
    assert_box(&annotation(&crop, "Offscreen")["box"], [0., 0., 80., 40.]);
    let image = image::open(path).unwrap().to_rgb8();
    assert_eq!(image.dimensions(), (80, 40));
    assert_eq!(image.get_pixel(40, 20).0, [231, 17, 83]);
    let path = dir.path().join("clipped.png");
    let crop = command(
        &mut state,
        json!({"action":"screenshot","annotate":true,"selector":"#clipped","path":path}),
    )
    .await;
    assert_box(
        &annotation(&crop, "Clipped")["box"],
        [-10.5, 0., 80.5, 40.25],
    );
    let image = image::open(path).unwrap().to_rgb8();
    assert_eq!(image.width(), 70);
    assert!(image.height().abs_diff(40) <= 1);
    assert_eq!(image.get_pixel(30, 20).0, [231, 17, 83]);
    let failure=Box::pin(execute_command(&json!({"action":"screenshot","annotate":true,"selector":"#hidden","path":dir.path().join("hidden.png")}),&mut state)).await;
    assert_eq!(failure["success"], false, "{failure}");
    assert!(!dir.path().join("hidden.png").exists());
    assert_eq!(evaluate(&state, None, "scrollY").await, 200);
    assert_eq!(
        evaluate(
            &state,
            None,
            "document.querySelectorAll('[id^=__agent_browser_annotations_]').length"
        )
        .await,
        0
    );
    command(&mut state, json!({"action":"close"})).await;
}

#[tokio::test]
#[ignore]
async fn e2e_screenshot_annotations_conditional_selected_document_scopes() {
    let (_env, dir) = binding_test_env();
    let mut state = DaemonState::new();
    command(&mut state, json!({"action":"launch","headless":true})).await;
    command(&mut state,json!({"action":"setcontent","html":"<button>Top</button><iframe id='child' srcdoc='<button>Child</button>'></iframe>"})).await;
    let top=command(&mut state,json!({"action":"screenshot","annotate":true,"ifChanged":true,"path":dir.path().join("top.png")})).await;
    // The main snapshot already participates in both documents.
    annotation(&top, "Top");
    annotation(&top, "Child");
    assert_eq!(top["data"]["revision"], 1);
    command(&mut state, json!({"action":"frame","selector":"#child"})).await;
    let child=command(&mut state,json!({"action":"screenshot","annotate":true,"ifChanged":true,"path":dir.path().join("child.png")})).await;
    annotation(&child, "Child");
    assert_eq!(
        child["data"]["revision"], 1,
        "selected-document identity must distinguish the same participating document set"
    );
    command(&mut state, json!({"action":"mainframe"})).await;
    let again=command(&mut state,json!({"action":"screenshot","annotate":true,"ifChanged":true,"path":dir.path().join("unchanged.png")})).await;
    assert_eq!(again["data"]["revision"], 2);
    assert_eq!(again["data"]["changed"], false, "{again}");
    assert!(!dir.path().join("unchanged.png").exists());
    command(&mut state, json!({"action":"close"})).await;
}

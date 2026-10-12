//! Device verify against fake `xcrun`, `emulator`, `adb` and `maestro`
//! scripts that log their argv, so these run on every machine.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;

use sun_core::SessionId;

use super::*;

const XCRUN: &str = r#"
case "$2" in
  list) cat <<'J'
{"devices":{
 "com.apple.CoreSimulator.SimRuntime.iOS-17-0":[{"name":"iPhone 15","udid":"O","deviceTypeIdentifier":"type.iPhone-15"}],
 "com.apple.CoreSimulator.SimRuntime.iOS-18-2":[{"name":"iPad Air","udid":"A","deviceTypeIdentifier":"type.iPad"},
   {"name":"iPhone 16","udid":"B","deviceTypeIdentifier":"type.iPhone-16"},
   {"name":"sun-01OLD","udid":"C","deviceTypeIdentifier":"type.iPhone-16"}],
 "com.apple.CoreSimulator.SimRuntime.watchOS-11-0":[{"name":"Apple Watch","udid":"W","deviceTypeIdentifier":"type.watch"}]}}
J
  ;;
  create) echo NEW-UDID ;;
  io) if [ "$6" = - ]; then printf 'JPEGlive'; else printf 'PNGfinal' > "$5"; fi ;;
esac
"#;

const EMULATOR: &str = r#"
case "$1" in
  -list-avds) printf 'INFO    | Storing crashdata in /tmp\nPixel_7_API_34\nSmall\n' ;;
  -avd) exec sleep 60 ;;
esac
"#;

const ADB: &str = r#"
case "$1" in
  version) echo "Android Debug Bridge version 1.0.41" ;;
  devices) printf 'List of devices attached\nemulator-5554\tdevice\n' ;;
  -s) case "$3" in
    wait-for-device) until grep -q '^emulator ' "$log"; do sleep 0.05; done ;;
    pull) printf 'PNGfinal' > "$5" ;;
    exec-out) [ "$4" = screenrecord ] && exec sh "$(dirname "$log")/video.sh"; printf 'PNGlive' ;;
  esac ;;
esac
exit 0
"#;

const IDB: &str = r#"
[ "$1" = video-stream ] && exec sh "$(dirname "$log")/video.sh"
[ "$1" = describe ] && echo '{"screen_dimensions":{"width":1170,"density":2.0}}'
exit 0
"#;

const MAESTRO: &str = r#"
[ "$1" = --version ] && { echo "Checking for updates"; echo "1.39.0"; exit 0; }
printf 'PNGlogin' > login.png
mkdir -p shots && printf 'PNGcart' > shots/cart.png
echo "token is $API_TOKEN"
[ "$FLOW" = hold ] && until [ -f "$RELEASE" ]; do sleep 0.05; done
[ "$FLOW" = fail ] && { echo "Assertion is false: Welcome is visible"; exit 1; }
echo "Flow passed"
"#;

struct Fakes {
    root: PathBuf,
    cwd: PathBuf,
    devices: Devices,
}

impl Fakes {
    fn new() -> Self {
        // Canonical, like a checked worktree (macOS temp is behind a symlink).
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("sun-device-{}", SessionId::generate()));
        let bin = root.join("bin");
        let cwd = root.join("work/task");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(cwd.join("Sun.app")).unwrap();
        std::fs::write(cwd.join("flow.yaml"), "appId: dev.sun\n---\n- launchApp\n").unwrap();
        std::fs::write(cwd.join("app.apk"), "apk").unwrap();
        let log = root.join("calls.log");
        let fake = |name: &str, body: &str| {
            let path = bin.join(name);
            let script = format!(
                "#!/bin/sh\nlog='{}'\necho \"{name} $*\" >> \"$log\"\n{body}",
                log.display()
            );
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let config = DeviceConfig {
            enabled: true,
            idb: fake("idb", IDB),
            xcrun: fake("xcrun", XCRUN),
            emulator: fake("emulator", EMULATOR),
            adb: fake("adb", ADB),
            maestro: fake("maestro", MAESTRO),
            max_running: 1,
            kvm: None,
        };
        Self {
            devices: Devices::new(config),
            cwd,
            root,
        }
    }

    /// Calls logged by the fakes, minus the tool probes.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| {
                !l.contains(" list") && !l.contains("-list-avds") && !l.ends_with("version")
            })
            .map(|l| l.replace(&self.cwd.display().to_string(), "<cwd>"))
            .collect()
    }

    fn request(&self, platform: DevicePlatform, app: &str, flow_result: &str) -> DeviceRequest {
        DeviceRequest {
            session_id: SessionId::generate(),
            cwd: self.cwd.clone(),
            platform,
            device: None,
            build: Some("echo building && touch built".into()),
            app: app.into(),
            flow: "flow.yaml".into(),
            env: BTreeMap::from([
                ("API_TOKEN".into(), "s3cr3t-value".into()),
                ("FLOW".into(), flow_result.into()),
            ]),
            timeout_s: 60,
        }
    }

    fn flow(&self, platform: Option<DevicePlatform>, app: Option<&str>) -> DeviceFlowRequest {
        DeviceFlowRequest {
            cwd: self.cwd.clone(),
            platform,
            device: None,
            app: app.map(PathBuf::from),
            flow: "appId: dev.sun\n---\n- tapOn: Login\n".into(),
            timeout_s: 60,
        }
    }

    async fn verify(&self, request: &DeviceRequest) -> Result<DeviceResult> {
        self.devices.verify(&self.root.join("tmp"), request).await
    }
}

impl Drop for Fakes {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn names(result: &DeviceResult) -> Vec<(&str, &[u8])> {
    result
        .screenshots
        .iter()
        .map(|s| (s.name.as_str(), s.png.as_slice()))
        .collect()
}

#[tokio::test]
async fn device_tools_are_detected_with_their_devices() {
    let fakes = Fakes::new();
    let caps = fakes.devices.detect().await;
    assert!(caps.ios_simulator.ready, "{caps:?}");
    assert_eq!(
        caps.ios_simulator.devices,
        [
            "iPhone 16 (iOS 18.2)",
            "iPhone 15 (iOS 17.0)",
            "iPad Air (iOS 18.2)"
        ]
    );
    assert!(caps.android_emulator.ready, "{caps:?}");
    assert_eq!(caps.android_emulator.devices, ["Pixel_7_API_34", "Small"]);
    assert_eq!(caps.maestro.version.as_deref(), Some("1.39.0"));
}

#[tokio::test]
async fn missing_device_tools_are_reported_with_the_fix() {
    let missing = PathBuf::from("/nonexistent/sun-tool");
    let devices = Devices::new(DeviceConfig {
        enabled: true,
        idb: missing.clone(),
        xcrun: missing.clone(),
        emulator: missing.clone(),
        adb: missing.clone(),
        maestro: missing,
        max_running: 1,
        kvm: Some("/nonexistent/kvm".into()),
    });
    let caps = devices.detect().await;
    for (tool, fix) in [
        (&caps.ios_simulator, "xcode-select"),
        (&caps.android_emulator, "ANDROID_HOME"),
        (&caps.maestro, "get.maestro.mobile.dev"),
    ] {
        assert!(!tool.ready);
        assert!(tool.devices.is_empty());
        assert!(tool.detail.as_deref().unwrap().contains(fix), "{tool:?}");
    }

    // A verify on such a host fails clearly, before building anything.
    let fakes = Fakes::new();
    let request = fakes.request(DevicePlatform::Ios, "Sun.app", "pass");
    let err = devices
        .verify(&fakes.root.join("tmp"), &request)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Unavailable(m) if m.contains("Maestro")),
        "{err}"
    );
    assert!(!fakes.cwd.join("built").exists());

    // KVM missing on a Linux host blocks the emulator only.
    let kvm = Devices::new(DeviceConfig {
        kvm: Some("/nonexistent/kvm".into()),
        ..fakes.devices.config.clone()
    });
    let caps = kvm.detect().await;
    assert!(caps.ios_simulator.ready && !caps.android_emulator.ready);
    assert!(caps.android_emulator.detail.unwrap().contains("kvm"));
}

#[tokio::test]
async fn ios_device_verify_builds_boots_runs_the_flow_and_deletes_the_simulator() {
    let fakes = Fakes::new();
    let mut request = fakes.request(DevicePlatform::Ios, "Sun.app", "pass");
    request.device = Some("iPhone 15".into());
    let result = fakes.verify(&request).await.unwrap();

    assert!(result.passed, "{}", result.output);
    assert_eq!(result.device, "iPhone 15 (iOS 17.0)");
    assert_eq!(
        names(&result),
        [
            ("login.png", &b"PNGlogin"[..]),
            ("cart.png", b"PNGcart"),
            ("sun-final.png", b"PNGfinal"),
        ]
    );
    assert!(result.output.contains("building") && result.output.contains("Flow passed"));
    assert!(!result.output.contains("s3cr3t-value"), "{}", result.output);
    assert!(fakes.cwd.join("built").exists());

    let sim = format!("sun-{}", request.session_id);
    let tmp = fakes
        .root
        .join("tmp")
        .join(format!("device-{}", request.session_id));
    assert_eq!(
        fakes.calls(),
        [
            format!(
                "xcrun simctl create {sim} type.iPhone-15 com.apple.CoreSimulator.SimRuntime.iOS-17-0"
            ),
            "xcrun simctl bootstatus NEW-UDID -b".into(),
            "xcrun simctl install NEW-UDID <cwd>/Sun.app".into(),
            "maestro --device NEW-UDID test <cwd>/flow.yaml".into(),
            format!(
                "xcrun simctl io NEW-UDID screenshot {}/sun-final.png",
                tmp.display()
            ),
            "xcrun simctl shutdown NEW-UDID".into(),
            "xcrun simctl delete NEW-UDID".into(),
        ]
    );
    assert!(!tmp.exists(), "scratch dir removed");
}

#[tokio::test]
async fn android_device_verify_reports_a_failed_flow_and_still_tears_down() {
    let fakes = Fakes::new();
    let request = fakes.request(DevicePlatform::Android, "app.apk", "fail");
    let result = fakes.verify(&request).await.unwrap();

    assert!(!result.passed);
    assert_eq!(result.device, "Pixel_7_API_34");
    assert!(
        result.output.contains("Assertion is false"),
        "{}",
        result.output
    );
    assert_eq!(names(&result).last().unwrap().0, "sun-final.png");

    let calls = fakes.calls();
    let tmp = fakes
        .root
        .join("tmp")
        .join(format!("device-{}", request.session_id));
    // emulator-5554 is taken, so this one gets the next console port. The
    // emulator runs in the background, concurrently with `adb wait-for-device`
    // (which waits for its log line, so a loaded machine can't lose it), so
    // its log line may land anywhere after `adb devices`.
    let (emulator, calls): (Vec<_>, Vec<_>) =
        calls.into_iter().partition(|c| c.starts_with("emulator "));
    assert_eq!(
        emulator,
        ["emulator -avd Pixel_7_API_34 -port 5556 -read-only -no-window -no-audio -no-boot-anim"]
    );
    assert_eq!(
        calls,
        [
            "adb devices".to_owned(),
            r#"adb -s emulator-5556 wait-for-device shell while [ "$(getprop sys.boot_completed)" != 1 ]; do sleep 1; done"#.into(),
            "adb -s emulator-5556 install -r <cwd>/app.apk".into(),
            "maestro --device emulator-5556 test <cwd>/flow.yaml".into(),
            "adb -s emulator-5556 shell screencap -p /sdcard/sun-final.png".into(),
            format!("adb -s emulator-5556 pull /sdcard/sun-final.png {}/sun-final.png", tmp.display()),
            "adb -s emulator-5556 emu kill".into(),
        ]
    );
    assert!(
        fakes.devices.ports.lock().unwrap().is_empty(),
        "port released"
    );
}

#[tokio::test]
async fn device_verify_stops_at_a_failed_build_and_keeps_paths_inside_the_worktree() {
    let fakes = Fakes::new();
    let mut request = fakes.request(DevicePlatform::Ios, "Sun.app", "pass");
    request.build = Some("echo compile error >&2; exit 3".into());
    let result = fakes.verify(&request).await.unwrap();
    assert!(!result.passed);
    assert!(result.output.starts_with("build failed") && result.output.contains("compile error"));
    assert!(
        fakes.calls().is_empty(),
        "nothing booted: {:?}",
        fakes.calls()
    );

    request.build = None;
    for app in ["../escape.app", "/etc", "missing.app"] {
        request.app = app.into();
        let err = fakes.verify(&request).await.unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{app}: {err}");
    }
    std::os::unix::fs::symlink("/etc", fakes.cwd.join("link.app")).unwrap();
    request.app = "link.app".into();
    assert!(matches!(
        fakes.verify(&request).await,
        Err(Error::InvalidInput(_))
    ));

    request.app = "Sun.app".into();
    request.device = Some("Galaxy Fold".into());
    let err = fakes.verify(&request).await.unwrap_err();
    assert!(
        matches!(&err, Error::InvalidInput(m) if m.contains("iPhone 16 (iOS 18.2)")),
        "{err}"
    );
    assert!(fakes.calls().is_empty());
}

#[tokio::test]
async fn live_view_shows_and_drives_the_running_device() {
    let fakes = Fakes::new();
    let release = fakes.root.join("release");
    for (platform, app, mime, image) in [
        (
            DevicePlatform::Android,
            "app.apk",
            "image/png",
            &b"PNGlive"[..],
        ),
        (
            DevicePlatform::Ios,
            "Sun.app",
            "image/jpeg",
            &b"JPEGlive"[..],
        ),
    ] {
        let _ = std::fs::remove_file(&release);
        let mut request = fakes.request(platform, app, "hold");
        request
            .env
            .insert("RELEASE".into(), release.display().to_string());
        let verify = fakes.verify(&request);
        let watch = async {
            let screen = loop {
                match fakes.devices.screen("task").await {
                    Ok(screen) => break screen,
                    Err(Error::NotFound(_)) => tokio::time::sleep(Duration::from_millis(50)).await,
                    Err(e) => panic!("{e}"),
                }
            };
            assert_eq!((screen.mime.as_str(), &screen.image[..]), (mime, image));
            assert!(screen.input, "adb, or idb for the iOS Simulator");
            let inputs = [
                DeviceInput::Tap { x: 10, y: 20 },
                DeviceInput::Text {
                    text: "it's me".into(),
                },
                DeviceInput::Key {
                    key: DeviceKey::Enter,
                },
            ];
            for input in &inputs {
                fakes.devices.input("task", input).await.unwrap();
            }
            let back = DeviceInput::Key {
                key: DeviceKey::Back,
            };
            let sent = fakes.devices.input("task", &back).await;
            assert_eq!(sent.is_ok(), platform == DevicePlatform::Android);
            let bad = DeviceInput::Text {
                text: "héllo".into(),
            };
            assert!(matches!(
                fakes.devices.input("task", &bad).await,
                Err(Error::InvalidInput(_))
            ));
            std::fs::write(&release, "").unwrap();
        };
        let (result, ()) = tokio::join!(verify, watch);
        assert!(result.unwrap().passed);
        assert!(matches!(
            fakes.devices.screen("task").await,
            Err(Error::NotFound(_))
        ));
    }
    let calls = fakes.calls();
    for call in [
        "adb -s emulator-5556 exec-out screencap -p",
        "adb -s emulator-5556 shell input tap 10 20",
        r"adb -s emulator-5556 shell input text 'it'\''s%sme'",
        "adb -s emulator-5556 shell input keyevent 66",
        "xcrun simctl io NEW-UDID screenshot --type=jpeg -",
        // Pixels → points at the fake's density 2.
        "idb ui tap 5 10 --udid NEW-UDID",
        "idb ui text it's me --udid NEW-UDID",
        "idb ui key 40 --udid NEW-UDID",
    ] {
        assert!(calls.iter().any(|c| c == call), "{call} not in {calls:#?}");
    }
}

#[tokio::test]
async fn devices_disabled_reports_not_ready() {
    let fakes = Fakes::new();
    let off = Devices::new(DeviceConfig {
        enabled: false,
        ..fakes.devices.config.clone()
    });
    let caps = off.detect().await;
    for tool in [&caps.ios_simulator, &caps.android_emulator, &caps.maestro] {
        assert!(!tool.ready && tool.devices.is_empty(), "{tool:?}");
        assert!(tool.detail.as_deref().unwrap().contains("--no-devices"));
    }
    let request = fakes.request(DevicePlatform::Android, "app.apk", "pass");
    let err = off
        .verify(&fakes.root.join("tmp"), &request)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Unavailable(m) if m.contains("VPS")),
        "{err}"
    );
    let flow = fakes.flow(Some(DevicePlatform::Android), None);
    assert!(matches!(
        off.flow(&fakes.root.join("tmp"), &flow).await,
        Err(Error::Unavailable(_))
    ));
    assert!(fakes.calls().is_empty(), "no tool ran: {:?}", fakes.calls());
    assert!(!fakes.cwd.join("built").exists());
}

#[tokio::test]
async fn agent_flow_boots_a_held_device_until_released() {
    let fakes = Fakes::new();
    let tmp = fakes.root.join("tmp");
    // Nothing up and no platform: nothing to drive.
    let nothing = fakes.flow(None, None);
    assert!(matches!(
        fakes.devices.flow(&tmp, &nothing).await,
        Err(Error::NotFound(_))
    ));

    let boot = fakes.flow(Some(DevicePlatform::Android), Some("app.apk"));
    let result = fakes.devices.flow(&tmp, &boot).await.unwrap();
    assert!(result.passed, "{}", result.output);
    assert_eq!(result.device, "Pixel_7_API_34");
    assert!(
        fakes.devices.screen("task").await.is_ok(),
        "held for the task"
    );

    // The next call reuses it; the one slot is taken, so a verify of another
    // task would wait, and `release` gives it back.
    let again = fakes.devices.flow(&tmp, &nothing).await.unwrap();
    assert!(again.passed);
    assert_eq!(fakes.devices.slots.available_permits(), 0);
    fakes.devices.release("task").await;
    assert_eq!(fakes.devices.slots.available_permits(), 1);
    assert!(matches!(
        fakes.devices.screen("task").await,
        Err(Error::NotFound(_))
    ));

    let calls: Vec<_> = fakes
        .calls()
        .into_iter()
        .filter(|c| !c.starts_with("emulator ") && !c.contains("wait-for-device"))
        .collect();
    assert_eq!(
        calls,
        [
            "adb devices",
            "adb -s emulator-5556 install -r <cwd>/app.apk",
            "maestro --device emulator-5556 test flow.yaml",
            "adb -s emulator-5556 exec-out screencap -p",
            "maestro --device emulator-5556 test flow.yaml",
            "adb -s emulator-5556 emu kill",
        ]
    );
    assert!(!tmp.exists() || std::fs::read_dir(&tmp).unwrap().next().is_none());
}

/// A canned H.264 Annex B stream: SPS, PPS, an IDR picture in two slices,
/// then two P pictures (one slice each).
const SPS_PPS_IDR: &[u8] = &[
    0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80, 0, 0, 0, 1, 0x65, 0x88,
    0x84, 0, 0, 1, 0x65, 0x21, 0x11,
];
const P1: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a, 0x02];
const P2: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a, 0x03];

#[test]
fn h264_is_cut_into_access_units() {
    let stream = [b"junk".as_slice(), SPS_PPS_IDR, P1, P2].concat();
    let want = vec![SPS_PPS_IDR.to_vec(), P1.to_vec(), P2.to_vec()];
    // At once, and a byte at a time (start codes straddle the pushes).
    let mut whole = video::AccessUnits::default();
    let mut units = whole.push(&stream);
    units.extend(whole.flush());
    assert_eq!(units, want);
    let mut bytes = video::AccessUnits::default();
    let mut units: Vec<_> = stream.iter().flat_map(|b| bytes.push(&[*b])).collect();
    assert!(bytes.pending());
    units.extend(bytes.flush());
    assert_eq!(units, want);
    assert!(!bytes.pending() && bytes.flush().is_empty());
}

#[tokio::test]
async fn live_view_streams_h264_from_the_running_device() {
    let fakes = Fakes::new();
    let octal = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|b| format!("\\{b:03o}"))
            .collect::<String>()
    };
    // The encoder writes a picture, pauses, writes the next, then exits.
    std::fs::write(
        fakes.root.join("video.sh"),
        format!(
            "printf '{}'; sleep 0.3; printf '{}'\n",
            octal(&[SPS_PPS_IDR, P1].concat()),
            octal(P2)
        ),
    )
    .unwrap();
    let release = fakes.root.join("release");
    for (platform, app) in [
        (DevicePlatform::Android, "app.apk"),
        (DevicePlatform::Ios, "Sun.app"),
    ] {
        let _ = std::fs::remove_file(&release);
        let mut request = fakes.request(platform, app, "hold");
        request
            .env
            .insert("RELEASE".into(), release.display().to_string());
        let verify = fakes.verify(&request);
        let watch = async {
            let mut frames = Vec::new();
            let ended = loop {
                match fakes.devices.video("task", "viewer-1").await {
                    Ok(video) => {
                        assert!(video.input);
                        frames.extend(video.frames.into_iter().map(|f| f.data));
                    }
                    Err(Error::NotFound(_)) => tokio::time::sleep(Duration::from_millis(50)).await,
                    Err(e) => break e,
                }
            };
            assert_eq!(frames, [SPS_PPS_IDR, P1, P2]);
            assert!(matches!(ended, Error::Unavailable(m) if m.contains("ended")));
            let bad = fakes.devices.video("task", "").await;
            assert!(matches!(bad, Err(Error::InvalidInput(_))));
            std::fs::write(&release, "").unwrap();
        };
        let (result, ()) = tokio::join!(verify, watch);
        assert!(result.unwrap().passed);
    }
    let calls = fakes.calls();
    for call in [
        "adb -s emulator-5556 exec-out screenrecord --output-format=h264 --bit-rate=4000000 -",
        "idb video-stream --format h264 --udid NEW-UDID",
    ] {
        assert!(calls.iter().any(|c| c == call), "{call} not in {calls:#?}");
    }
}

//! `sun-mod-sandbox`: jails itself, then runs a mod (Linux; see
//! `sun_mod::sandbox::linux`). Install it next to the `sun` binary.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    sun_mod::sandbox::linux::launch()
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("sun-mod-sandbox: Linux only");
    std::process::ExitCode::from(126)
}

//! `sun-fake-runtime <script.jsonl>`: replays a script of `SunEvent`s on
//! stdout, one JSON line each, so tests drive the engine without a real model.
//! The script format is documented in `sun_runtime::fake`.

use std::io::{BufRead, Write};
use std::process::ExitCode;

use sun_core::{Role, SunEvent};
use sun_runtime::fake::parse_script;

fn main() -> ExitCode {
    let Some(path) = std::env::args_os().nth(1) else {
        eprintln!("usage: sun-fake-runtime <script.jsonl>");
        return ExitCode::from(2);
    };
    let script = match std::fs::read_to_string(&path) {
        Ok(text) => parse_script(&text),
        Err(e) => Err(sun_core::Error::InvalidInput(format!(
            "{}: {e}",
            path.to_string_lossy()
        ))),
    };
    let script = match script {
        Ok(script) => script,
        Err(e) => {
            eprintln!("sun-fake-runtime: {e}");
            return ExitCode::from(2);
        }
    };

    // stdin: the envelope, then steers; each a JSON string. Steers are echoed.
    // Like a real CLI, read the envelope before acting: a short script that
    // exited first would make the kernel's envelope write fail (EPIPE).
    let mut envelope = String::new();
    let _ = std::io::stdin().lock().read_line(&mut envelope);
    std::thread::spawn(|| {
        for raw in std::io::stdin().lock().lines().map_while(Result::ok) {
            let text = serde_json::from_str::<String>(&raw).unwrap_or(raw);
            emit(&SunEvent::Text {
                role: Role::User,
                text,
            });
        }
    });

    for mut line in script {
        std::thread::sleep(line.delay);
        if let (SunEvent::LimitHit { reset_at, .. }, Some(reset_in)) =
            (&mut line.event, line.reset_in)
        {
            *reset_at = chrono::TimeDelta::from_std(reset_in)
                .ok()
                .map(|d| chrono::Utc::now() + d);
        }
        if let (SunEvent::FileChanged { path }, Some(text)) = (&line.event, &line.write)
            && let Err(e) = std::fs::write(path, text)
        {
            eprintln!("sun-fake-runtime: writing {path}: {e}");
            return ExitCode::from(2);
        }
        match line.event {
            SunEvent::Exited { code: Some(code) } => std::process::exit(code),
            SunEvent::Exited { code: None } => std::process::abort(),
            event => emit(&event),
        }
    }
    ExitCode::SUCCESS
}

/// Prints one event line; a closed stdout means the session is gone.
fn emit(event: &SunEvent) {
    let Ok(json) = serde_json::to_string(event) else {
        return;
    };
    if writeln!(std::io::stdout().lock(), "{json}").is_err() {
        std::process::exit(1);
    }
}

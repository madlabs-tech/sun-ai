//! The `extension` environment (mod API 1.2): an extension that declares
//! `provides.exec_envs` runs the session's commands, over its JSON-RPC
//! `exec_env.*` actions (**Adapter** from [`ExecEnv`] to [`ModLink`]).
//!
//! The extension never gets a secret: `prepare` sends the names of
//! the session's env vars, not their values, and may only add new vars, never
//! `PATH` or [`EXEC_SHIMS_ENV`], so the host never runs a file the sandboxed
//! mod wrote. The agent CLI thus stays on the host; gates and other `exec`
//! commands run inside, with their env (the step chose this environment).

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use sun_core::host_protocol::{ExecOutput, ExecRequest};
use sun_core::redact::redact;
use sun_core::{
    EXEC_SHIMS_ENV, Error, ExtensionEnvSpec, ModCall, ModLink, Result, SessionId, SessionSpec,
    exec_env_action,
};

use super::ExecEnv;

/// The extension envs a host can open: the kernel's mods and the
/// `<mod>.<kind>` names they declared. Only a host in the kernel's process
/// has one ([`HostConfig::mod_envs`](crate::HostConfig::mod_envs)).
#[derive(Clone)]
pub struct ModEnvs {
    /// The kernel's mod host.
    pub link: Arc<dyn ModLink>,
    /// `<mod>.<kind>` of every loaded extension's `provides.exec_envs`.
    pub names: Vec<String>,
}

impl fmt::Debug for ModEnvs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModEnvs")
            .field("names", &self.names)
            .finish_non_exhaustive()
    }
}

/// One session's environment, served by an extension.
pub struct ModEnv {
    link: Arc<dyn ModLink>,
    spec: ExtensionEnvSpec,
    session_id: SessionId,
}

impl ModEnv {
    /// The env `spec` names for `session_id`, if `envs` serves it.
    pub fn open(
        envs: Option<&ModEnvs>,
        spec: &ExtensionEnvSpec,
        session_id: SessionId,
    ) -> Result<Self> {
        let envs = envs.filter(|e| e.names.contains(&spec.name) && spec.split().is_some());
        let Some(envs) = envs else {
            return Err(Error::Unavailable(format!(
                "exec env {}: no installed extension serves it on this host",
                spec.name
            )));
        };
        Ok(Self {
            link: envs.link.clone(),
            spec: spec.clone(),
            session_id,
        })
    }

    async fn call(&self, action: &str, mut params: Value) -> Result<Value> {
        let (mod_name, kind) = self.spec.split().unwrap_or_default();
        params["kind"] = json!(kind);
        params["session_id"] = json!(self.session_id);
        self.link
            .call(ModCall {
                mod_name: mod_name.into(),
                method: action.into(),
                params,
                idem_key: None,
            })
            .await
    }
}

#[async_trait]
impl ExecEnv for ModEnv {
    async fn prepare(&self, spec: &mut SessionSpec) -> Result<()> {
        let names: Vec<&String> = spec.env.keys().collect();
        let params = json!({
            "worktree": spec.worktree,
            "env_names": names,
            "config": self.spec.config,
        });
        let reply = self.call(exec_env_action::PREPARE, params).await?;
        let added = match reply.get("env") {
            None | Some(Value::Null) => Default::default(),
            Some(env) => {
                serde_json::from_value::<std::collections::BTreeMap<String, String>>(env.clone())
                    .map_err(|e| Error::Protocol(format!("{}: prepare: {e}", self.spec.name)))?
            }
        };
        for (name, value) in added {
            if name == "PATH" || name == EXEC_SHIMS_ENV || spec.env.contains_key(&name) {
                return Err(Error::Protocol(format!(
                    "{}: prepare may not set {name}",
                    self.spec.name
                )));
            }
            spec.env.insert(name, value);
        }
        Ok(())
    }

    async fn exec(&self, request: &ExecRequest) -> Result<ExecOutput> {
        // ponytail: one mod call, so the mod host's call timeout (60 s) caps
        // a command; give `ModCall` its own timeout when gates need longer.
        let params = serde_json::to_value(request).map_err(|e| Error::Protocol(e.to_string()))?;
        let reply = self.call(exec_env_action::EXEC, params).await?;
        let mut out: ExecOutput = serde_json::from_value(reply)
            .map_err(|e| Error::Protocol(format!("{}: exec: {e}", self.spec.name)))?;
        let secrets: Vec<&str> = request.env.values().map(String::as_str).collect();
        out.output = redact(&out.output, &secrets);
        Ok(out)
    }

    async fn teardown(&self) -> Result<()> {
        self.call(exec_env_action::TEARDOWN, json!({}))
            .await
            .map(drop)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use sun_core::ExecEnvSpec;

    use super::super::testing::{fixture, gate};
    use super::super::{detect, open};
    use super::*;
    use crate::HostConfig;

    /// The echo test mod (`sun-mod`'s `sun-echo-mod`), built and loaded as
    /// an extension with exec env `echo`.
    async fn echo_extension() -> sun_mod::ModHost {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let mut args = vec!["build", "-q", "-p", "sun-mod", "--bin", "sun-echo-mod"];
        if !cfg!(debug_assertions) {
            args.push("--release");
        }
        let built = std::process::Command::new(cargo).args(&args).status();
        assert!(built.unwrap().success(), "cargo {args:?} failed");
        // `target/<profile>/`, next to this test's `deps/`.
        let exe = std::env::current_exe().unwrap();
        let bin = exe.parent().unwrap().parent().unwrap().join("sun-echo-mod");
        let yaml = format!(
            "name: echo\nkind: extension\nversion: 0.1.0\napi: \">=1.2 <2.0\"\n\
             entry: {{ command: \"{}\" }}\nprovides: {{ exec_envs: [echo] }}\n",
            bin.display()
        );
        let host = sun_mod::ModHost::new(sun_mod::HostConfig::default());
        let manifest = sun_mod::parse_manifest(&yaml).unwrap();
        host.add(manifest, std::env::temp_dir()).await.unwrap();
        host
    }

    #[tokio::test]
    async fn mod_exec_env_runs_a_command() {
        let mods = Arc::new(echo_extension().await);
        let spec = ExecEnvSpec::Extension(ExtensionEnvSpec {
            name: "echo.echo".into(),
            config: json!({ "size": "small" }),
        });
        spec.validate().unwrap();
        let (dir, mut session) = fixture(spec.clone());
        let mut config = HostConfig::new(dir.join("host"));
        let id = session.session_id;
        let missing = open(&spec, id, &config).err().unwrap().to_string();
        assert!(missing.contains("no installed extension"), "{missing}");

        config.mod_envs = Some(ModEnvs {
            link: mods.clone(),
            names: vec!["echo.echo".into()],
        });
        let kinds = detect(&config).await;
        assert!(kinds.iter().any(|e| e.kind == "echo.echo" && e.ready));
        let env = open(&spec, id, &config).unwrap();
        env.prepare(&mut session).await.unwrap();
        assert_eq!(session.env["ECHO_ENV"], "echo");
        assert_eq!(session.env["SUN_TOKEN"], "s3cret-token-42");

        let command = "echo \"in=$IN_ECHO_ENV secret=$GATE_SECRET\"; exit 3";
        let out = env.exec(&gate(&session, command)).await.unwrap();
        assert_eq!(out.code, Some(3));
        assert!(out.output.contains(&format!("in={id}")), "{}", out.output);
        assert!(!out.output.contains("g4te-value-77"), "{}", out.output);
        env.teardown().await.unwrap();
        env.teardown().await.unwrap();
        mods.shutdown().await;
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

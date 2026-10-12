//! `sun user list|invite|reinvite|set-password|deactivate|activate` and
//! `sun smtp set|show|test`, through the running kernel (admin token).
//! Secrets are read from stdin, never argv.

use std::io::BufRead;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::client::Client;

#[derive(Subcommand)]
pub enum UserCommand {
    /// Members: who they are, and whether their invite is pending.
    List,
    /// Invite someone: they get an email with a one-time sign-in code, then
    /// pick a password. Needs `sun smtp set` first.
    Invite {
        /// Their email.
        #[arg(long)]
        email: String,
        /// Their username (approvals' `who: user:<name>`).
        #[arg(long)]
        name: String,
        /// Their role in Main: viewer, member, admin or owner.
        #[arg(long, default_value = "viewer")]
        role: String,
        /// Make them an owner (only an owner can).
        #[arg(long)]
        owner: bool,
    },
    /// Send a new code (a lost invite or a forgotten password): their
    /// password is cleared and their sessions end.
    Reinvite {
        /// Member id or email.
        member: String,
    },
    /// Set a member's password (lockout recovery, yours included): typed at
    /// a prompt, or read from stdin when it isn't a terminal. Their sessions
    /// end.
    SetPassword {
        /// Member id or email.
        member: String,
        /// Make them change it at their next sign-in.
        #[arg(long)]
        must_change: bool,
    },
    /// Deactivate a member: they can't sign in, and their sessions end.
    Deactivate {
        /// Member id or email.
        member: String,
    },
    /// Reactivate a deactivated member.
    Activate {
        /// Member id or email.
        member: String,
    },
}

#[derive(Subcommand)]
pub enum SmtpCommand {
    /// Save the mail server. With --password-stdin the SMTP password is read
    /// from stdin (an empty line clears it); without, the saved one is kept.
    Set {
        /// Server host name.
        #[arg(long)]
        host: String,
        /// Server port (587 for starttls, 465 for tls).
        #[arg(long, default_value_t = 587)]
        port: u16,
        /// Login name; omit for no authentication.
        #[arg(long, default_value = "")]
        username: String,
        /// The From address.
        #[arg(long)]
        from: String,
        /// starttls or tls.
        #[arg(long, default_value = "starttls")]
        tls: String,
        /// Read the SMTP password from stdin.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Show the mail server (never the password).
    Show,
    /// Send a test mail to your own email.
    Test,
}

/// The member `who` names (an id or an email).
fn member(client: &Client, who: &str) -> Result<Value> {
    let members = client.get("/members")?;
    members
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| {
            m["id"] == who
                || m["email"]
                    .as_str()
                    .is_some_and(|e| e.eq_ignore_ascii_case(who))
        })
        .cloned()
        .with_context(|| format!("no member `{who}`"))
}

/// The id of the member `who` names (an id or an email).
fn member_id(client: &Client, who: &str) -> Result<String> {
    let m = member(client, who)?;
    Ok(m["id"].as_str().unwrap_or_default().to_owned())
}

fn status(member: &Value) -> &'static str {
    if !member["deactivated_at"].is_null() {
        return "deactivated";
    }
    match (member["email"].is_null(), member["has_password"].as_bool()) {
        (true, _) => "needs email",
        (false, Some(false)) if member["issuer"] == "local" => "invited",
        _ => "active",
    }
}

pub fn run_user(client: &Client, command: UserCommand) -> Result<()> {
    match command {
        UserCommand::List => {
            for m in client.get("/members")?.as_array().into_iter().flatten() {
                let owner = if m["is_owner"] == true { "owner " } else { "" };
                println!(
                    "{}  {:<16} {:<32} {owner}{}",
                    m["id"].as_str().unwrap_or("?"),
                    m["username"].as_str().unwrap_or("?"),
                    m["email"].as_str().unwrap_or("-"),
                    status(m),
                );
            }
        }
        UserCommand::Invite {
            email,
            name,
            role,
            owner,
        } => {
            let body = json!({ "email": email, "username": name, "role": role,
                "is_owner": owner });
            let member = client.post("/members", &body)?;
            println!(
                "invited {} ({}): the code is in their mail",
                member["username"].as_str().unwrap_or("?"),
                member["email"].as_str().unwrap_or("?")
            );
        }
        UserCommand::Reinvite { member } => {
            let id = member_id(client, &member)?;
            client.post(&format!("/members/{id}/invite"), &json!({}))?;
            println!("sent a new code to {member}");
        }
        UserCommand::SetPassword {
            member,
            must_change,
        } => {
            let id = member_id(client, &member)?;
            let password = read_password()?;
            let body = json!({ "password": password, "must_change": must_change });
            client.put(&format!("/members/{id}/password"), &body)?;
            println!("password set for {member}; their sessions ended");
        }
        UserCommand::Deactivate { member } => {
            let id = member_id(client, &member)?;
            client.post(&format!("/members/{id}/deactivate"), &json!({}))?;
            println!("deactivated {member}");
        }
        UserCommand::Activate { member } => {
            let id = member_id(client, &member)?;
            client.post(&format!("/members/{id}/activate"), &json!({}))?;
            println!("reactivated {member}");
        }
    }
    Ok(())
}

/// A password from a prompt with echo off (`stty`), or the first line of
/// stdin when it isn't a terminal.
fn read_password() -> Result<String> {
    use std::io::IsTerminal;
    use std::process::{Command, Stdio};
    let tty = std::io::stdin().is_terminal();
    let stty = |arg: &str| {
        Command::new("stty")
            .arg(arg)
            .stdin(Stdio::inherit())
            .status()
    };
    if tty {
        eprint!("New password: ");
        // ponytail: no echo control without `stty` (Windows); it echoes there.
        let _ = stty("-echo");
    }
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    if tty {
        let _ = stty("echo");
        eprintln!();
    }
    read?;
    let password = line.trim_end_matches(['\r', '\n']).to_owned();
    if password.is_empty() {
        bail!("no password given");
    }
    Ok(password)
}

pub fn run_smtp(client: &Client, command: SmtpCommand) -> Result<()> {
    match command {
        SmtpCommand::Set {
            host,
            port,
            username,
            from,
            tls,
            password_stdin,
        } => {
            let mut body = json!({
                "host": host, "port": port, "username": username, "from": from, "tls": tls,
            });
            if password_stdin {
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line)?;
                body["password"] = json!(line.trim_end_matches(['\r', '\n']));
            }
            let saved = client.put("/smtp", &body)?;
            println!("saved; password stored: {}", saved["has_password"]);
        }
        SmtpCommand::Show => {
            let view = client.get("/smtp")?;
            let smtp = &view["smtp"];
            if smtp.is_null() {
                bail!("no mail server yet: run `sun smtp set`");
            }
            println!(
                "{}:{} ({}) from {} as {}; password stored: {}",
                smtp["host"].as_str().unwrap_or("?"),
                smtp["port"],
                smtp["tls"].as_str().unwrap_or("?"),
                smtp["from"].as_str().unwrap_or("?"),
                smtp["username"]
                    .as_str()
                    .filter(|u| !u.is_empty())
                    .unwrap_or("(no login)"),
                view["has_password"],
            );
        }
        SmtpCommand::Test => {
            let sent = client.post("/smtp/test", &json!({}))?;
            println!(
                "sent a test mail to {}",
                sent["sent_to"].as_str().unwrap_or("?")
            );
        }
    }
    Ok(())
}

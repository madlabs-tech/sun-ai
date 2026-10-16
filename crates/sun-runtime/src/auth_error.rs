//! Shared auth-failure classifier: does a CLI's error text say its login or
//! key is no longer good? The host re-probes readiness right after a session
//! that ended on one, so the runtime shows "needs login" without waiting for
//! the 5-minute probe. A false positive only costs one cheap probe.

use std::sync::LazyLock;

use regex::Regex;

/// Wording of Claude Code (`Invalid API key · Please run /login`, `OAuth
/// token has expired`), Codex (`401 Unauthorized`, `Not logged in`) and
/// OpenCode/provider errors; matched case-insensitively.
const PATTERN: &str = r"(?i)not logged in|please (run )?/?log ?in|log ?in again|invalid (api[ _-]?key|x-api-key|bearer|token|credentials)|authentication[_ ](failed|error|required)|(oauth|access) token (has )?expired|unauthori[sz]ed|\b401\b|no (credentials|auth(entication)?) (found|configured)";

static AUTH: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(PATTERN).ok());

/// `true` when `text` reads like a failed login or rejected key.
pub fn is_auth_error(text: &str) -> bool {
    AUTH.as_ref().is_some_and(|re| re.is_match(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_wording_of_each_cli_matches_and_limits_do_not() {
        assert!(AUTH.is_some(), "the pattern compiles");
        for text in [
            "Invalid API key · Please run /login",
            "OAuth token has expired. Please obtain a new token or refresh your existing token.",
            "unexpected status 401 Unauthorized: Missing bearer or basic authentication in header",
            "Not logged in. Run `codex login`.",
            "AuthenticationError: authentication_error: invalid x-api-key",
            "No credentials found for provider anthropic",
        ] {
            assert!(is_auth_error(text), "{text}");
        }
        for text in [
            "Claude AI usage limit reached|1760000000",
            "rate limit exceeded, retry in 20s",
            "error: tests failed (4010 assertions)",
            "file not found: login.rs",
        ] {
            assert!(!is_auth_error(text), "{text}");
        }
    }
}

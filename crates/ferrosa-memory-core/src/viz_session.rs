//! Browser sessions for the viz dashboard.
//!
//! ## Why a cookie
//!
//! viz serves the whole graph and authenticated nobody. A browser cannot set an
//! `Authorization` header on a WebSocket handshake, so the dashboard needs some
//! credential the browser will attach by itself — which means a cookie.
//!
//! The cookie's classic weakness is cross-site WebSocket hijacking: handshakes
//! are not subject to CORS, so a hostile page can open a socket and the browser
//! will attach the cookie. That hole was real here and is now closed by the
//! `Origin` check on the upgrade, which is what makes a cookie safe to use. The
//! two controls are a pair, not alternatives: the origin check stops a foreign
//! page connecting, the cookie stops an unauthenticated local one.
//!
//! Cookies buy one thing a bearer token cannot: `HttpOnly`, so injected script
//! cannot read the credential.
//!
//! Non-browser clients — curl, the Rust workbench — send no cookie and are
//! expected to present `Authorization` instead. Both paths check the same
//! credentials, so this is the same authentication the other ports use rather
//! than a parallel scheme.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Name of the session cookie. Prefixed so a similarly-named cookie on the same
/// host cannot be mistaken for it.
pub const VIZ_COOKIE: &str = "ferrosa_viz_session";

/// How long a dashboard session lives without being re-issued.
pub const VIZ_SESSION_TTL: Duration = Duration::from_secs(60 * 60 * 8);

/// Issued dashboard sessions.
///
/// Deliberately in-process and non-persistent: a session should not outlive the
/// daemon that authenticated it, and viz has no clustering story that would
/// need shared state.
pub struct VizSessions {
    entries: Mutex<HashMap<String, Instant>>,
    ttl: Duration,
}

impl VizSessions {
    pub fn new() -> Self {
        Self::with_ttl(VIZ_SESSION_TTL)
    }

    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// Mint a session token. Called only after credentials have been checked.
    pub fn issue(&self) -> String {
        let token = uuid::Uuid::new_v4().simple().to_string();
        let mut entries = self.entries.lock().expect("viz session lock poisoned");
        entries.insert(token.clone(), Instant::now() + self.ttl);
        token
    }

    /// Is this token a live session? Expired entries are dropped as they are
    /// found, so an abandoned browser cannot leave a session valid forever.
    pub fn validate(&self, token: &str) -> bool {
        let mut entries = self.entries.lock().expect("viz session lock poisoned");
        match entries.get(token) {
            Some(expiry) if *expiry > Instant::now() => true,
            Some(_) => {
                entries.remove(token);
                false
            }
            None => false,
        }
    }

    /// Drop a session, so signing out actually signs out.
    pub fn revoke(&self, token: &str) {
        self.entries
            .lock()
            .expect("viz session lock poisoned")
            .remove(token);
    }

    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .expect("viz session lock poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for VizSessions {
    fn default() -> Self {
        Self::new()
    }
}

/// The `Set-Cookie` value for a freshly issued session.
///
/// `Secure` is conditional because the flag makes a browser DROP the cookie
/// over plain HTTP, and the dashboard is normally reached over loopback HTTP.
/// Setting it unconditionally would lock users out of their own dashboard.
pub fn set_cookie_header(token: &str, secure: bool) -> String {
    let mut cookie = format!(
        "{VIZ_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        VIZ_SESSION_TTL.as_secs()
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Read the session token out of a `Cookie` header.
///
/// Matches on the whole name. A cookie called `not_ferrosa_viz_session` must
/// not satisfy a check for `ferrosa_viz_session`, and a substring search would
/// let it.
pub fn session_token_from_headers(headers: &[(String, String)]) -> Option<String> {
    let cookie_header = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("cookie"))
        .map(|(_, value)| value.as_str())?;

    cookie_header.split(';').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name.trim() == VIZ_COOKIE).then(|| value.trim().to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie_headers(value: &str) -> Vec<(String, String)> {
        vec![("Cookie".to_owned(), value.to_owned())]
    }

    #[test]
    fn a_freshly_issued_session_validates() {
        let sessions = VizSessions::new();
        let token = sessions.issue();
        assert!(sessions.validate(&token));
    }

    #[test]
    fn an_unknown_token_does_not_validate() {
        let sessions = VizSessions::new();
        sessions.issue();
        assert!(!sessions.validate("not-a-real-token"));
        assert!(!sessions.validate(""));
    }

    #[test]
    fn an_expired_session_is_refused_and_dropped() {
        // An abandoned browser must not leave a session valid forever.
        let sessions = VizSessions::with_ttl(Duration::from_millis(0));
        let token = sessions.issue();
        assert_eq!(sessions.len(), 1);

        assert!(!sessions.validate(&token), "a zero-TTL session is expired");
        assert_eq!(sessions.len(), 0, "the expired entry must be dropped");
    }

    #[test]
    fn revoking_a_session_ends_it() {
        let sessions = VizSessions::new();
        let token = sessions.issue();
        sessions.revoke(&token);
        assert!(!sessions.validate(&token));
    }

    #[test]
    fn tokens_are_distinct() {
        let sessions = VizSessions::new();
        let first = sessions.issue();
        let second = sessions.issue();
        assert_ne!(first, second);
        assert!(first.len() >= 32, "token is too short to be unguessable");
    }

    #[test]
    fn the_cookie_is_httponly_and_samesite_strict() {
        // HttpOnly is the reason a cookie was chosen over a bearer token:
        // injected script cannot read it.
        let cookie = set_cookie_header("abc123", false);
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Strict"), "{cookie}");
        assert!(cookie.contains("Max-Age="), "{cookie}");
        assert!(
            !cookie.contains("Secure"),
            "Secure over plain HTTP makes the browser DROP the cookie: {cookie}"
        );
    }

    #[test]
    fn the_cookie_is_secure_under_tls() {
        let cookie = set_cookie_header("abc123", true);
        assert!(cookie.contains("; Secure"), "{cookie}");
    }

    #[test]
    fn the_token_is_read_from_among_other_cookies() {
        let headers = cookie_headers(&format!("theme=dark; {VIZ_COOKIE}=tok123; other=1"));
        assert_eq!(
            session_token_from_headers(&headers).as_deref(),
            Some("tok123")
        );
    }

    #[test]
    fn a_similarly_named_cookie_is_not_mistaken_for_the_session() {
        // A substring search would accept this and hand a session to anyone who
        // could set any cookie on the host.
        let headers = cookie_headers("not_ferrosa_viz_session=attacker");
        assert_eq!(session_token_from_headers(&headers), None);
    }

    #[test]
    fn absent_or_empty_cookie_headers_yield_nothing() {
        assert_eq!(session_token_from_headers(&[]), None);
        assert_eq!(session_token_from_headers(&cookie_headers("")), None);
        assert_eq!(
            session_token_from_headers(&cookie_headers("theme=dark")),
            None
        );
    }
}

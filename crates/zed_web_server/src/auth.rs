use std::{
    collections::HashMap,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::State,
    http::{HeaderMap, Request, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use hmac::{Hmac, Mac as _};
use rand::RngCore as _;
use sha2::Sha256;

use crate::AppState;

pub const AUTH_COOKIE: &str = "zed_web_session";
const AUTH_SESSION_SECONDS: u64 = 30 * 24 * 60 * 60;
const LOGIN_WINDOW: Duration = Duration::from_secs(5 * 60);
const LOGIN_BLOCK: Duration = Duration::from_secs(15 * 60);
const LOGIN_ATTEMPTS: u32 = 5;

#[derive(Default)]
pub struct LoginLimiter {
    attempts: Mutex<HashMap<IpAddr, LoginAttempts>>,
}

struct LoginAttempts {
    started_at: Instant,
    failures: u32,
    blocked_until: Option<Instant>,
}

impl LoginLimiter {
    pub fn is_allowed(&self, address: IpAddr) -> bool {
        let now = Instant::now();
        let mut attempts = self.attempts.lock().expect("login limiter lock poisoned");
        attempts.retain(|_, attempt| {
            attempt.blocked_until.map_or(
                now.duration_since(attempt.started_at) <= LOGIN_WINDOW,
                |until| until > now,
            )
        });
        attempts
            .get(&address)
            .and_then(|attempt| attempt.blocked_until)
            .is_none_or(|until| until <= now)
    }

    pub fn record_failure(&self, address: IpAddr) {
        let now = Instant::now();
        let mut attempts = self.attempts.lock().expect("login limiter lock poisoned");
        let attempt = attempts.entry(address).or_insert(LoginAttempts {
            started_at: now,
            failures: 0,
            blocked_until: None,
        });
        if now.duration_since(attempt.started_at) > LOGIN_WINDOW {
            *attempt = LoginAttempts {
                started_at: now,
                failures: 0,
                blocked_until: None,
            };
        }
        attempt.failures += 1;
        if attempt.failures >= LOGIN_ATTEMPTS {
            attempt.blocked_until = Some(now + LOGIN_BLOCK);
        }
    }

    pub fn record_success(&self, address: IpAddr) {
        self.attempts
            .lock()
            .expect("login limiter lock poisoned")
            .remove(&address);
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn signature(token: &str, payload: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

pub fn token_matches(supplied: &str, expected: &str) -> bool {
    constant_time_eq(supplied.as_bytes(), expected.as_bytes())
}

pub fn new_session(token: &str) -> String {
    let issued_at = now_seconds();
    let mut nonce = [0_u8; 24];
    rand::rng().fill_bytes(&mut nonce);
    let payload = format!("{issued_at}.{}", hex::encode(nonce));
    format!("{payload}.{}", signature(token, &payload))
}

fn valid_session(token: &str, session: &str) -> bool {
    let mut parts = session.splitn(3, '.');
    let Some(issued) = parts.next() else {
        return false;
    };
    let Some(nonce) = parts.next() else {
        return false;
    };
    let Some(supplied_signature) = parts.next() else {
        return false;
    };
    let Ok(issued_at) = issued.parse::<u64>() else {
        return false;
    };
    let now = now_seconds();
    if nonce.is_empty()
        || issued_at > now.saturating_add(60)
        || now.saturating_sub(issued_at) > AUTH_SESSION_SECONDS
    {
        return false;
    }
    token_matches(
        supplied_signature,
        &signature(token, &format!("{issued}.{nonce}")),
    )
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then(|| value.to_string()))
}

pub fn authenticated(headers: &HeaderMap, token: &str) -> bool {
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        && token_matches(value, token)
    {
        return true;
    }
    cookie(headers, AUTH_COOKIE).is_some_and(|session| valid_session(token, &session))
}

pub fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    origin
        .parse::<axum::http::Uri>()
        .ok()
        .and_then(|uri| {
            uri.authority()
                .map(|authority| authority.as_str().to_owned())
        })
        .is_some_and(|authority| authority.eq_ignore_ascii_case(host))
}

#[cfg(test)]
mod tests {
    use super::{LoginLimiter, new_session, same_origin, token_matches, valid_session};
    use axum::http::{HeaderMap, HeaderValue, header};
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn token_comparison_rejects_different_lengths_and_contents() {
        assert!(token_matches("secret", "secret"));
        assert!(!token_matches("secret", "secret-extra"));
        assert!(!token_matches("secreu", "secret"));
    }

    #[test]
    fn generated_session_is_valid() {
        let token = "server-secret";
        assert!(valid_session(token, &new_session(token)));
        assert!(!valid_session("different-secret", &new_session(token)));
    }

    #[test]
    fn rejects_cross_origin_requests() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("zed.example:8090"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://other.example"),
        );
        assert!(!same_origin(&headers));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://zed.example:8090"),
        );
        assert!(same_origin(&headers));
    }

    #[test]
    fn blocks_repeated_login_failures() {
        let limiter = LoginLimiter::default();
        let address = IpAddr::V4(Ipv4Addr::LOCALHOST);
        for _ in 0..5 {
            assert!(limiter.is_allowed(address));
            limiter.record_failure(address);
        }
        assert!(!limiter.is_allowed(address));
        limiter.record_success(address);
        assert!(limiter.is_allowed(address));
    }
}

pub async fn require_auth<B>(
    State(state): State<AppState>,
    request: Request<B>,
    next: Next<B>,
) -> Response {
    let path = request.uri().path();
    if matches!(path, "/login" | "/logout" | "/favicon.ico") {
        return next.run(request).await;
    }
    if authenticated(request.headers(), &state.auth_token) {
        let changes_state = !matches!(
            *request.method(),
            axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
        );
        if changes_state && !same_origin(request.headers()) {
            return (StatusCode::FORBIDDEN, "cross-origin request rejected").into_response();
        }
        return next.run(request).await;
    }

    let accepts_html = request
        .headers()
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/html"));
    if request.method() == axum::http::Method::GET && accepts_html {
        return Redirect::temporary("/login").into_response();
    }

    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer realm=\"Zed Web\"")],
        "authentication required",
    )
        .into_response()
}

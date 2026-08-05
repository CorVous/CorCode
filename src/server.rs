//! HTTP server: routes, the session gate, serving, and graceful shutdown.

use std::future::Future;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context as _, Result};
use axum::Router;
use axum::extract::{Form, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use log::error;
use serde::Deserialize;
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use ulid::Ulid;

use crate::auth::gate::{Gate, SignIn};
use crate::auth::session;
use crate::config::Config;
use crate::store::{ChatStore, ContainerLiveness, Event, runtime_status};
use crate::ui::{self, page};

/// Name of the cookie carrying the session (ADR-0003).
pub const SESSION_COOKIE: &str = "corcode_session";

/// Where visitors without a session are sent.
const LOGIN_PATH: &str = "/login";

/// The unauthenticated liveness probe (ADR-0003).
const HEALTH_PATH: &str = "/health";

/// Build the application's routes. Every route is gated except the handful
/// [`is_public`] names, so a route added here is protected by default
/// (ADR-0003).
pub fn router<L>(config: &Config, liveness: L) -> Result<Router>
where
    L: ContainerLiveness + Send + Sync + 'static,
{
    let gate = Arc::new(Gate::new(config)?);
    let chats = Arc::new(Chats {
        store: ChatStore::new(&config.data_dir),
        liveness,
        workspace_image: config.workspace_image.clone(),
    });
    Ok(Router::new()
        .merge(console_routes(chats))
        .merge(session_routes(Arc::clone(&gate)))
        .layer(from_fn_with_state(gate, require_session))
        .layer(from_fn(same_origin)))
}

/// The read-only console over the dataset (ADR-0008).
fn console_routes<L>(chats: Arc<Chats<L>>) -> Router
where
    L: ContainerLiveness + Send + Sync + 'static,
{
    Router::new()
        .route("/", get(console))
        .route(ui::CHATS_PATH, get(chat_list))
        .route(&format!("{}/{{chat_id}}", ui::CHATS_PATH), get(chat_view))
        .route(ui::HTMX_PATH, get(htmx))
        .with_state(chats)
}

/// Signing in, signing out, and saying the process is alive (ADR-0003).
fn session_routes(gate: Arc<Gate>) -> Router {
    Router::new()
        .route(ui::LOGOUT_PATH, post(logout_all))
        .route(HEALTH_PATH, get(health))
        .route(LOGIN_PATH, get(login_form).post(submit_login))
        .with_state(gate)
}

/// What the console handlers read: the dataset, who is live, and the pinned
/// image the status line names.
struct Chats<L> {
    store: ChatStore,
    liveness: L,
    workspace_image: String,
}

impl<L: ContainerLiveness + Sync> Chats<L> {
    /// Every chat on disk paired with the status it has right now.
    async fn survey(&self) -> Result<Vec<ui::Chat>> {
        let live = self.liveness.live_chat_ids().await?;
        Ok(self
            .store
            .scan()?
            .into_iter()
            .map(|manifest| {
                let status = runtime_status(&manifest, &live);
                (manifest, status)
            })
            .collect())
    }

    /// One chat and its whole event log, or nothing if the dataset holds no
    /// such chat. A pure read: opening a chat never touches a container
    /// (ADR-0007).
    async fn open(&self, chat_id: &Ulid) -> Result<Option<(ui::Chat, Vec<Event>)>> {
        let chat_id = chat_id.to_string();
        let manifest = match self.store.read_manifest(&chat_id) {
            Ok(manifest) => manifest,
            Err(failure) if failure.is_missing() => return Ok(None),
            Err(failure) => return Err(failure.into()),
        };
        let events = self.store.read_events(&chat_id)?;
        let live = self.liveness.live_chat_ids().await?;
        let status = runtime_status(&manifest, &live);
        Ok(Some(((manifest, status), events)))
    }
}

/// The only requests that reach a handler without a session: the liveness
/// probe and the login form itself (ADR-0003).
fn is_public(method: &Method, path: &str) -> bool {
    match path {
        HEALTH_PATH => method == Method::GET,
        LOGIN_PATH => matches!(*method, Method::GET | Method::POST),
        _ => false,
    }
}

/// Unauthenticated liveness probe (ADR-0003).
async fn health() -> &'static str {
    "ok"
}

/// The one screen: status line, new-chat form, grouped chat list (ADR-0008).
async fn console<L>(State(chats): State<Arc<Chats<L>>>) -> Response
where
    L: ContainerLiveness + Send + Sync + 'static,
{
    match chats.survey().await {
        Ok(survey) => Html(ui::console_page(&survey, &chats.workspace_image)).into_response(),
        Err(failure) => broken_invariant(&failure),
    }
}

/// The chat list alone, for htmx to swap into the console.
async fn chat_list<L>(State(chats): State<Arc<Chats<L>>>) -> Response
where
    L: ContainerLiveness + Send + Sync + 'static,
{
    match chats.survey().await {
        Ok(survey) => Html(ui::chat_list(&survey)).into_response(),
        Err(failure) => broken_invariant(&failure),
    }
}

/// One chat's event log (ADR-0006). A chat id is a ULID and nothing else, so
/// the path segment is parsed before the store sees it: no request can name a
/// file, inside `chats/` or out of it.
async fn chat_view<L>(State(chats): State<Arc<Chats<L>>>, Path(chat_id): Path<String>) -> Response
where
    L: ContainerLiveness + Send + Sync + 'static,
{
    let Ok(chat_id) = chat_id.parse::<Ulid>() else {
        return no_such_chat();
    };
    match chats.open(&chat_id).await {
        Ok(Some(((manifest, status), events))) => {
            Html(ui::chat_page(&manifest, status, &events)).into_response()
        }
        Ok(None) => no_such_chat(),
        Err(failure) => broken_invariant(&failure),
    }
}

/// The one answer a chat id that is not a chat gets, saying nothing about
/// what is or is not on disk.
fn no_such_chat() -> Response {
    (StatusCode::NOT_FOUND, "No such chat.\n").into_response()
}

/// The htmx bundle, served from the binary so no page reaches a CDN.
async fn htmx() -> Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/javascript; charset=utf-8"),
        )],
        ui::HTMX,
    )
        .into_response()
}

/// Show the operator what broke instead of repairing or hiding it
/// (ADR-0007 rule 5).
fn broken_invariant(failure: &anyhow::Error) -> Response {
    error!("{failure:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{failure:#}\n")).into_response()
}

/// Turn away requests without a valid session, sliding the window forward
/// on the ones that carry an ageing cookie. The refreshed cookie is minted
/// under the key in force before the handler runs, so a handler that
/// rotates the key still logs this device out.
async fn require_session(State(gate): State<Arc<Gate>>, request: Request, next: Next) -> Response {
    if is_public(request.method(), request.uri().path()) {
        return next.run(request).await;
    }
    let now = SystemTime::now();
    let Some(session) =
        session_cookie(request.headers()).and_then(|cookie| gate.recognise(cookie, now))
    else {
        return Redirect::to(LOGIN_PATH).into_response();
    };
    let refreshed = session
        .needs_refresh(now)
        .then(|| cookie_header(&gate.issue_cookie(now)));
    let mut response = next.run(request).await;
    if let Some(cookie) = refreshed {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    response
}

/// Refuse mutating requests a browser marked as coming from elsewhere.
async fn same_origin(request: Request, next: Next) -> Response {
    let mutating = !matches!(*request.method(), Method::GET | Method::HEAD);
    if mutating && !origin_matches_host(request.headers()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

/// The login form, as served to anyone.
async fn login_form() -> Html<String> {
    login_page(None)
}

/// Weigh submitted credentials and hand out a session on success.
async fn submit_login(
    State(gate): State<Arc<Gate>>,
    Form(credentials): Form<Credentials>,
) -> Response {
    let now = SystemTime::now();
    match gate
        .sign_in(&credentials.username, &credentials.password, now)
        .await
    {
        SignIn::Granted => (
            StatusCode::SEE_OTHER,
            [
                (header::LOCATION, HeaderValue::from_static("/")),
                (header::SET_COOKIE, cookie_header(&gate.issue_cookie(now))),
            ],
        )
            .into_response(),
        SignIn::Refused => (
            StatusCode::UNAUTHORIZED,
            login_page(Some("That username and password did not match.")),
        )
            .into_response(),
        SignIn::RateLimited => (
            StatusCode::TOO_MANY_REQUESTS,
            login_page(Some("Too many failed attempts. Wait a moment.")),
        )
            .into_response(),
    }
}

/// Rotate the signing key, ending every session everywhere (ADR-0003).
async fn logout_all(State(gate): State<Arc<Gate>>) -> Response {
    if let Err(failure) = gate.rotate_key() {
        error!("could not rotate the signing key: {failure:#}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Redirect::to(LOGIN_PATH).into_response()
}

/// The credentials a login form submits.
#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: String,
}

/// The session value a request carries, if it carries one.
fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|cookie| cookie.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value)
}

/// A `Set-Cookie` value for a session: plain HTTP over the tailnet, so no
/// `Secure` flag (ADR-0003).
fn cookie_header(session: &str) -> HeaderValue {
    let max_age = session::LIFETIME.as_secs();
    let cookie =
        format!("{SESSION_COOKIE}={session}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}");
    HeaderValue::from_str(&cookie).expect("a session cookie is a valid header value")
}

/// Whether the browser's stated origin, if any, is this same site. Only the
/// authority is compared: the app binds the tailnet address directly and is
/// reached over plain HTTP (ADR-0003), so a scheme mismatch would be the
/// proxy deployment this app does not have.
fn origin_matches_host(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let host = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok());
    origin
        .to_str()
        .ok()
        .and_then(|origin| origin.split_once("://"))
        .is_some_and(|(_, authority)| Some(authority) == host)
}

/// The login screen, optionally reporting why the last attempt failed.
fn login_page(notice: Option<&str>) -> Html<String> {
    let notice = notice.map_or_else(String::new, |notice| {
        format!("<p role=\"alert\">{notice}</p>")
    });
    Html(page(
        "Sign in to CorCode",
        &format!(
            "{notice}<form method=\"post\" action=\"{LOGIN_PATH}\">\
             <p><label>Username <input name=\"username\" autocomplete=\"username\" autofocus></label></p>\
             <p><label>Password <input type=\"password\" name=\"password\" autocomplete=\"current-password\"></label></p>\
             <p><button type=\"submit\">Sign in</button></p></form>"
        ),
    ))
}

/// Serve `router` on `listener` until `shutdown` resolves, then wait for
/// in-flight requests to finish.
pub async fn serve<S>(listener: TcpListener, router: Router, shutdown: S) -> Result<()>
where
    S: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .context("HTTP server failed")
}

/// Resolve when the process is asked to stop: Ctrl-C or SIGTERM.
#[cfg(unix)]
pub async fn shutdown_signal() {
    let mut interrupt = signal(SignalKind::interrupt()).expect("SIGINT handler should install");
    let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM handler should install");
    tokio::select! {
        _ = interrupt.recv() => {},
        _ = terminate.recv() => {},
    }
}

/// Resolve when the process is asked to stop: Ctrl-C (SIGTERM has no Unix
/// signal handling to fall back to here).
#[cfg(not(unix))]
pub async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Ctrl-C handler should install");
}

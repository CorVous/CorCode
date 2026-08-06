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

use crate::acp::AcpTransport;
use crate::auth::gate::{Gate, SignIn};
use crate::auth::session;
use crate::chats::{Chats, PromptError, WantedChat};
use crate::config::Config;
use crate::plane::ContainerPlane;
use crate::store::ContainerLiveness;
use crate::ui::{self, page};

/// Name of the cookie carrying the session (ADR-0003).
pub const SESSION_COOKIE: &str = "corcode_session";

/// Where visitors without a session are sent.
const LOGIN_PATH: &str = "/login";

/// The unauthenticated liveness probe (ADR-0003).
const HEALTH_PATH: &str = "/health";

/// The path parameter every per-chat route is spelled with.
const CHAT_ID: &str = "{chat_id}";

/// Build the application's routes. Every route is gated except the handful
/// [`is_public`] names, so a route added here is protected by default
/// (ADR-0003).
pub fn router<P, T>(config: &Config, chats: Chats<P, T>) -> Result<Router>
where
    P: ContainerPlane + ContainerLiveness + Send + Sync + 'static,
    T: AcpTransport + Send + Sync + 'static,
{
    let gate = Arc::new(Gate::new(config)?);
    Ok(Router::new()
        .merge(console_routes(Arc::new(chats)))
        .merge(session_routes(Arc::clone(&gate)))
        .layer(from_fn_with_state(gate, require_session))
        .layer(from_fn(same_origin)))
}

/// The console over the dataset and the one form it posts (ADR-0008).
fn console_routes<P, T>(chats: Arc<Chats<P, T>>) -> Router
where
    P: ContainerPlane + ContainerLiveness + Send + Sync + 'static,
    T: AcpTransport + Send + Sync + 'static,
{
    Router::new()
        .route("/", get(console))
        .route(ui::CHATS_PATH, get(chat_list).post(create_chat))
        .route(&ui::chat_path(CHAT_ID), get(chat_view))
        .route(&ui::chat_events_path(CHAT_ID), get(chat_events))
        .route(&ui::chat_prompt_path(CHAT_ID), post(prompt_chat))
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
async fn console<P, T>(State(chats): State<Arc<Chats<P, T>>>) -> Response
where
    P: ContainerPlane + ContainerLiveness + Send + Sync + 'static,
    T: AcpTransport + Send + Sync + 'static,
{
    match chats.survey().await {
        Ok(survey) => Html(ui::console_page(
            &survey,
            chats.workspace_image(),
            chats.repos(),
        ))
        .into_response(),
        Err(failure) => broken_invariant(&failure),
    }
}

/// The chat list alone, for htmx to swap into the console.
async fn chat_list<P, T>(State(chats): State<Arc<Chats<P, T>>>) -> Response
where
    P: ContainerPlane + ContainerLiveness + Send + Sync + 'static,
    T: AcpTransport + Send + Sync + 'static,
{
    match chats.survey().await {
        Ok(survey) => Html(ui::chat_list(&survey)).into_response(),
        Err(failure) => broken_invariant(&failure),
    }
}

/// Cut a new chat and send the browser to it (ADR-0005). A refusal says what
/// was wrong with the request; a failure says only that there was one, and
/// leaves the whole of it in the log.
async fn create_chat<P, T>(
    State(chats): State<Arc<Chats<P, T>>>,
    Form(form): Form<NewChatForm>,
) -> Response
where
    P: ContainerPlane + ContainerLiveness + Send + Sync + 'static,
    T: AcpTransport + Send + Sync + 'static,
{
    match chats.create(form.into()).await {
        Ok(chat_id) => Redirect::to(&format!("{}/{chat_id}", ui::CHATS_PATH)).into_response(),
        Err(refusal) if refusal.is_refusal() => {
            (StatusCode::BAD_REQUEST, format!("{refusal}\n")).into_response()
        }
        Err(failure) => {
            error!("a chat could not be created: {failure:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The chat could not be created.\n",
            )
                .into_response()
        }
    }
}

/// What the new-chat form submits. An unticked checkbox sends nothing at all,
/// which is how the opt-out reads as false.
#[derive(Deserialize)]
struct NewChatForm {
    repo: String,
    base_branch: String,
    slug: String,
    direct_on_base: Option<String>,
}

impl From<NewChatForm> for WantedChat {
    fn from(form: NewChatForm) -> Self {
        Self {
            repo: form.repo,
            base_branch: form.base_branch,
            slug: form.slug,
            direct_on_base: form.direct_on_base.is_some(),
        }
    }
}

/// One chat's event log (ADR-0006). A chat id is a ULID and nothing else, so
/// the path segment is parsed before the store sees it: no request can name a
/// file, inside `chats/` or out of it.
async fn chat_view<P, T>(
    State(chats): State<Arc<Chats<P, T>>>,
    Path(chat_id): Path<String>,
) -> Response
where
    P: ContainerPlane + ContainerLiveness + Send + Sync + 'static,
    T: AcpTransport + Send + Sync + 'static,
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

/// One chat's event log on its own, for htmx to poll while the page is open
/// (ADR-0006: the log is read from disk, never from the connection).
async fn chat_events<P, T>(
    State(chats): State<Arc<Chats<P, T>>>,
    Path(chat_id): Path<String>,
) -> Response
where
    P: ContainerPlane + ContainerLiveness + Send + Sync + 'static,
    T: AcpTransport + Send + Sync + 'static,
{
    let Ok(chat_id) = chat_id.parse::<Ulid>() else {
        return no_such_chat();
    };
    rendered_log(&chats, &chat_id)
}

/// Put one prompt to a chat's agent and answer with the log it landed in, so
/// the page never navigates. The turn is awaited whole: the poll is what
/// shows it arriving.
async fn prompt_chat<P, T>(
    State(chats): State<Arc<Chats<P, T>>>,
    Path(chat_id): Path<String>,
    Form(form): Form<PromptForm>,
) -> Response
where
    P: ContainerPlane + ContainerLiveness + Send + Sync + 'static,
    T: AcpTransport + Send + Sync + 'static,
{
    let Ok(chat_id) = chat_id.parse::<Ulid>() else {
        return no_such_chat();
    };
    match chats.prompt(&chat_id, &form.prompt).await {
        Ok(()) => rendered_log(&chats, &chat_id),
        Err(refusal @ PromptError::NotConnected) => {
            (StatusCode::TOO_EARLY, format!("{refusal}.\n")).into_response()
        }
        Err(refusal @ PromptError::Busy) => {
            (StatusCode::CONFLICT, format!("{refusal}.\n")).into_response()
        }
        Err(failure @ PromptError::Unrecorded(_)) => {
            error!("a turn was lost: {:#}", anyhow::Error::new(failure));
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The turn could not be written down.\n",
            )
                .into_response()
        }
        Err(failure) => {
            error!("a turn broke: {:#}", anyhow::Error::new(failure));
            (StatusCode::BAD_GATEWAY, "The turn broke.\n").into_response()
        }
    }
}

/// What the prompt box submits.
#[derive(Deserialize)]
struct PromptForm {
    prompt: String,
}

/// The chat's log as it stands on disk right now.
fn rendered_log<P, T>(chats: &Chats<P, T>, chat_id: &Ulid) -> Response
where
    P: ContainerPlane + ContainerLiveness + Send + Sync + 'static,
    T: AcpTransport + Send + Sync + 'static,
{
    match chats.events(chat_id) {
        Ok(Some(events)) => Html(ui::event_log(&chat_id.to_string(), &events)).into_response(),
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

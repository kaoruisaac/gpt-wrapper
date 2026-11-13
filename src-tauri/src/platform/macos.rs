//! macOS native adapter (WKWebView via wry/Tauri + AppKit).
//!
//! Design notes (see .agent_temp/macos-arm64-port/PHASE_0_FINDINGS.md):
//! - JS -> Rust: a per-WebView `WKScriptMessageHandler` named `gptWrapper`.
//!   The source is taken from WebKit's `WKFrameInfo` (main frame only, real
//!   security origin), never from the page payload.
//! - Navigation identity: wry owns the `WKNavigationDelegate`; we interpose a
//!   forwarding proxy that maps each `WKNavigation` object to a monotonically
//!   increasing ID, so A -> B -> A, stale and duplicate callbacks are
//!   distinguishable exactly like WebView2 NavigationId.
//! - Media permission: a forwarding `WKUIDelegate` proxy grants microphone
//!   capture only to the tab's own provider origin in the main frame; camera,
//!   other origins and iframes are denied. The OS microphone (TCC) prompt is
//!   still shown by macOS and cannot be bypassed.
//! - Trusted input: synthesized AppKit `NSEvent` mouse down/up delivered to the
//!   WKWebView. WebKit treats them as native input (isTrusted + user
//!   activation), unlike a DOM `.click()`.
//! - All AppKit/WebKit calls happen on the main thread (`with_webview` and
//!   WebKit callbacks run there; other entry points require a marker).
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::{c_void, CStr},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject, Bool, ClassBuilder, NSObject, ProtocolObject, Sel},
    sel, AllocAnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, Message,
};
use objc2_app_kit::{
    NSApplication, NSEvent, NSEventModifierFlags, NSEventType, NSMenu, NSMenuItem, NSPanel,
    NSSound, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{
    NSError, NSNumber, NSObjectProtocol, NSPoint, NSProcessInfo, NSString,
};
use objc2_web_kit::{
    WKFrameInfo, WKMediaCaptureType, WKNavigation, WKNavigationDelegate, WKPermissionDecision,
    WKScriptMessage, WKScriptMessageHandler, WKSecurityOrigin, WKUIDelegate,
    WKUserContentController, WKWebView,
};
use tauri::{
    webview::NewWindowResponse, window::Window, AppHandle, Manager, Webview, WebviewBuilder,
    WebviewWindow, Wry,
};
use uuid::Uuid;

use crate::{
    background_project_switcher, create_tab_with_spec, external_link_url, link_tab_spec,
    project_settings::ProjectList, provider::Provider, refresh_tab_host, tab_provider,
    MenuAnchor, MAIN_WINDOW_LABEL,
};

/// The `window.webkit.messageHandlers.<name>` used by the shared JS bridge.
pub const MESSAGE_HANDLER_NAME: &str = "gptWrapper";

/// Native handle used to dispatch trusted input into the WebView that ran a script.
pub type ClickTarget = Webview;

// ---------------------------------------------------------------------------
// Tab identity stored on the WKWebView instance itself (lifetime-bound, so a
// recycled pointer can never inherit another tab's identity).
// ---------------------------------------------------------------------------

static TAB_ID_KEY: u8 = 0;
static NAVIGATION_PROXY_KEY: u8 = 0;
static UI_PROXY_KEY: u8 = 0;
static APP: OnceLock<AppHandle> = OnceLock::new();

unsafe fn set_associated(object: &AnyObject, key: &'static u8, value: &AnyObject) {
    objc2::ffi::objc_setAssociatedObject(
        object as *const AnyObject as *mut AnyObject,
        key as *const u8 as *const c_void,
        value as *const AnyObject as *mut AnyObject,
        objc2::ffi::OBJC_ASSOCIATION_RETAIN_NONATOMIC,
    );
}

unsafe fn get_associated(object: &AnyObject, key: &'static u8) -> *const AnyObject {
    objc2::ffi::objc_getAssociatedObject(
        object as *const AnyObject as *mut AnyObject,
        key as *const u8 as *const c_void,
    ) as *const AnyObject
}

fn tab_id_of(webview: &AnyObject) -> Option<usize> {
    let value = unsafe { get_associated(webview, &TAB_ID_KEY) };
    if value.is_null() {
        return None;
    }
    let number = unsafe { &*(value as *const NSNumber) };
    usize::try_from(number.integerValue()).ok()
}

fn register_tab(app: &AppHandle, webview: &WKWebView, tab_id: usize) {
    let _ = APP.set(app.clone());
    if tab_id_of(webview).is_some() {
        return;
    }
    let number = NSNumber::new_isize(tab_id as isize);
    unsafe { set_associated(webview, &TAB_ID_KEY, &number) };
}

fn registered_provider(webview: &AnyObject) -> Option<(usize, Provider)> {
    let tab_id = tab_id_of(webview)?;
    let app = APP.get()?;
    Some((tab_id, tab_provider(app, tab_id)?))
}

unsafe fn wk_webview<'a>(pointer: *mut c_void) -> &'a WKWebView {
    &*(pointer as *const WKWebView)
}

fn security_origin_source(origin: &WKSecurityOrigin) -> String {
    let (protocol, host, port) = unsafe {
        (
            origin.protocol().to_string(),
            origin.host().to_string(),
            origin.port(),
        )
    };
    if port > 0 {
        format!("{protocol}://{host}:{port}/")
    } else {
        format!("{protocol}://{host}/")
    }
}

fn webview_url(webview: &WKWebView) -> Option<url::Url> {
    let url = unsafe { webview.URL() }?;
    let text = url.absoluteString()?.to_string();
    url::Url::parse(&text).ok()
}

// ---------------------------------------------------------------------------
// Script execution and trusted input
// ---------------------------------------------------------------------------

/// Runs `script` via `evaluateJavaScript:` and returns the JSON-serialized
/// completion value on the main thread. WebKit reports exceptions, Promises
/// and `undefined` as an empty result, which is surfaced as `Err` so callers
/// fail closed (never "button absent" or "success").
pub fn eval_json<R, U>(webview: &Webview, script: String, on_result: R, on_unavailable: U)
where
    R: FnOnce(Result<String, String>, ClickTarget) + Send + 'static,
    U: FnOnce(String) + Send + 'static,
{
    let target = webview.clone();
    let slot = Mutex::new(Some(on_result));
    let result = webview.eval_with_callback(script, move |raw: String| {
        let Some(callback) = slot.lock().ok().and_then(|mut slot| slot.take()) else {
            return;
        };
        let result = if raw.is_empty() {
            Err("script threw, returned undefined/a Promise, or an unsupported value".to_string())
        } else {
            Ok(raw)
        };
        callback(result, target.clone());
    });
    if let Err(error) = result {
        on_unavailable(format!("evaluateJavaScript unavailable: {error}"));
    }
}

/// Dispatches a native left click at viewport CSS coordinates `(x, y)`.
/// The events are AppKit `NSEvent`s handed straight to the WKWebView, so
/// WebKit processes them as real user input (trusted events with user
/// activation) without moving the cursor or activating the app.
pub fn trusted_click(target: &ClickTarget, x: f64, y: f64) {
    if !x.is_finite() || !y.is_finite() {
        eprintln!("[gpt-wrapper] trusted click rejected: non-finite coordinates");
        return;
    }
    let result = target.with_webview(move |platform| unsafe {
        dispatch_native_click(wk_webview(platform.inner()), x, y);
    });
    if let Err(error) = result {
        eprintln!("[gpt-wrapper] trusted click WebView unavailable: {error}");
    }
}

unsafe fn dispatch_native_click(view: &WKWebView, x: f64, y: f64) {
    let Some(window) = view.window() else {
        eprintln!("[gpt-wrapper] trusted click skipped: WebView has no window");
        return;
    };
    let zoom = view.pageZoom();
    let zoom = if zoom.is_finite() && zoom > 0.0 { zoom } else { 1.0 };
    let bounds = view.bounds();
    let (vx, vy) = (x * zoom, y * zoom);
    if vx < 0.0 || vy < 0.0 || vx > bounds.size.width || vy > bounds.size.height {
        eprintln!("[gpt-wrapper] trusted click skipped: ({x:.1}, {y:.1}) outside the WebView");
        return;
    }
    let local = NSPoint::new(vx, if view.isFlipped() { vy } else { bounds.size.height - vy });
    let location = view.convertPoint_toView(local, None);
    let window_number = window.windowNumber();
    let timestamp = NSProcessInfo::processInfo().systemUptime();
    let event = |kind: NSEventType, pressure: f32| {
        NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            kind,
            location,
            NSEventModifierFlags::empty(),
            timestamp,
            window_number,
            None,
            0,
            1,
            pressure,
        )
    };
    let (Some(moved), Some(down), Some(up)) = (
        event(NSEventType::MouseMoved, 0.0),
        event(NSEventType::LeftMouseDown, 1.0),
        event(NSEventType::LeftMouseUp, 0.0),
    ) else {
        eprintln!("[gpt-wrapper] trusted click skipped: NSEvent creation failed");
        return;
    };
    view.mouseMoved(&moved);
    view.mouseDown(&down);
    view.mouseUp(&up);
}

// ---------------------------------------------------------------------------
// JS -> Rust message bridge
// ---------------------------------------------------------------------------

struct MessageHandlerIvars {
    tab_id: usize,
    handler: Box<dyn Fn(&str, &str)>,
    rejected_frame: Cell<bool>,
    rejected_payload: Cell<bool>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "GptWrapperScriptMessageHandler"]
    #[ivars = MessageHandlerIvars]
    struct ScriptMessageHandler;

    unsafe impl NSObjectProtocol for ScriptMessageHandler {}

    unsafe impl WKScriptMessageHandler for ScriptMessageHandler {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive(&self, _controller: &WKUserContentController, message: &WKScriptMessage) {
            let ivars = self.ivars();
            let frame: Retained<WKFrameInfo> = unsafe { message.frameInfo() };
            // Only the top-level document may talk to the host, as on Windows
            // where the adapters refuse to run inside iframes.
            if !unsafe { frame.isMainFrame() } {
                if !ivars.rejected_frame.replace(true) {
                    eprintln!("[gpt-wrapper] tab {}: message from a subframe rejected", ivars.tab_id);
                }
                return;
            }
            let source = security_origin_source(&*unsafe { frame.securityOrigin() });
            let body = unsafe { message.body() };
            let Some(text) = body.downcast_ref::<NSString>() else {
                if !ivars.rejected_payload.replace(true) {
                    eprintln!("[gpt-wrapper] tab {}: non-string message rejected", ivars.tab_id);
                }
                return;
            };
            (ivars.handler)(&source, &text.to_string());
        }
    }
);

impl ScriptMessageHandler {
    fn new(mtm: MainThreadMarker, tab_id: usize, handler: Box<dyn Fn(&str, &str)>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(MessageHandlerIvars {
            tab_id,
            handler,
            rejected_frame: Cell::new(false),
            rejected_payload: Cell::new(false),
        });
        unsafe { msg_send![super(this), init] }
    }
}

/// Delivers `(security origin of the main frame, string payload)` for every
/// `window.webkit.messageHandlers.gptWrapper.postMessage(...)`. Subframe and
/// non-string messages are dropped here; trust is decided by the caller.
pub fn install_message_listener<F>(webview: &Webview, tab_id: usize, handler: F) -> Result<(), String>
where
    F: Fn(&str, &str) + Send + 'static,
{
    webview
        .with_webview(move |platform| unsafe {
            let Some(mtm) = MainThreadMarker::new() else {
                eprintln!("[gpt-wrapper] tab {tab_id}: message bridge must be installed on the main thread");
                return;
            };
            let controller = &*(platform.controller() as *const WKUserContentController);
            let object = ScriptMessageHandler::new(mtm, tab_id, Box::new(handler));
            controller.addScriptMessageHandler_name(
                ProtocolObject::from_ref(&*object),
                &NSString::from_str(MESSAGE_HANDLER_NAME),
            );
            #[cfg(debug_assertions)]
            eprintln!("[gpt-wrapper] status listener installed on tab {tab_id}");
        })
        .map_err(|error| format!("failed to access WKWebView for tab {tab_id}: {error}"))
}

// ---------------------------------------------------------------------------
// Navigation identity (forwarding WKNavigationDelegate proxy)
// ---------------------------------------------------------------------------

struct NavigationProxyIvars {
    inner: Retained<AnyObject>,
    on_started: Box<dyn Fn(u64, url::Url)>,
    on_finished: Box<dyn Fn(u64, url::Url, bool)>,
    // WKNavigation objects are retained so their identity cannot be recycled.
    navigations: RefCell<Vec<(Retained<WKNavigation>, u64)>>,
    // Navigations WebKit reports without a WKNavigation object.
    anonymous: Cell<Option<u64>>,
    next_id: Cell<u64>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "GptWrapperNavigationProxy"]
    #[ivars = NavigationProxyIvars]
    struct NavigationProxy;

    impl NavigationProxy {
        #[unsafe(method(respondsToSelector:))]
        fn responds_to_selector(&self, selector: Sel) -> bool {
            let own: bool = unsafe { msg_send![super(self), respondsToSelector: selector] };
            own || unsafe { msg_send![&*self.ivars().inner, respondsToSelector: selector] }
        }

        #[unsafe(method(forwardingTargetForSelector:))]
        fn forwarding_target(&self, selector: Sel) -> *mut AnyObject {
            let inner = &self.ivars().inner;
            let responds: bool = unsafe { msg_send![&**inner, respondsToSelector: selector] };
            if responds {
                Retained::as_ptr(inner) as *mut AnyObject
            } else {
                std::ptr::null_mut()
            }
        }
    }

    unsafe impl NSObjectProtocol for NavigationProxy {}

    unsafe impl WKNavigationDelegate for NavigationProxy {
        #[unsafe(method(webView:didStartProvisionalNavigation:))]
        fn did_start(&self, webview: &WKWebView, navigation: Option<&WKNavigation>) {
            self.forward2(sel!(webView:didStartProvisionalNavigation:), webview, navigation);
            let id = self.begin(navigation);
            if let Some(url) = webview_url(webview) {
                (self.ivars().on_started)(id, url);
            }
        }

        #[unsafe(method(webView:didReceiveServerRedirectForProvisionalNavigation:))]
        fn did_redirect(&self, webview: &WKWebView, navigation: Option<&WKNavigation>) {
            self.forward2(
                sel!(webView:didReceiveServerRedirectForProvisionalNavigation:),
                webview,
                navigation,
            );
            if let Some(id) = self.lookup(navigation) {
                if let Some(url) = webview_url(webview) {
                    (self.ivars().on_started)(id, url);
                }
            }
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish(&self, webview: &WKWebView, navigation: Option<&WKNavigation>) {
            self.forward2(sel!(webView:didFinishNavigation:), webview, navigation);
            self.complete(webview, navigation, true);
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail(&self, webview: &WKWebView, navigation: Option<&WKNavigation>, error: &NSError) {
            self.forward3(sel!(webView:didFailNavigation:withError:), webview, navigation, error);
            self.complete(webview, navigation, false);
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(
            &self,
            webview: &WKWebView,
            navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            self.forward3(
                sel!(webView:didFailProvisionalNavigation:withError:),
                webview,
                navigation,
                error,
            );
            self.complete(webview, navigation, false);
        }
    }
);

impl NavigationProxy {
    fn new(
        mtm: MainThreadMarker,
        inner: Retained<AnyObject>,
        on_started: Box<dyn Fn(u64, url::Url)>,
        on_finished: Box<dyn Fn(u64, url::Url, bool)>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(NavigationProxyIvars {
            inner,
            on_started,
            on_finished,
            navigations: RefCell::new(Vec::new()),
            anonymous: Cell::new(None),
            next_id: Cell::new(0),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn inner_responds(&self, selector: Sel) -> bool {
        unsafe { msg_send![&*self.ivars().inner, respondsToSelector: selector] }
    }

    fn forward2(&self, selector: Sel, webview: &WKWebView, navigation: Option<&WKNavigation>) {
        if !self.inner_responds(selector) {
            return;
        }
        let inner = &*self.ivars().inner;
        unsafe {
            let _: () = match selector {
                s if s == sel!(webView:didStartProvisionalNavigation:) => {
                    msg_send![inner, webView: webview, didStartProvisionalNavigation: navigation]
                }
                s if s == sel!(webView:didReceiveServerRedirectForProvisionalNavigation:) => {
                    msg_send![inner, webView: webview, didReceiveServerRedirectForProvisionalNavigation: navigation]
                }
                _ => msg_send![inner, webView: webview, didFinishNavigation: navigation],
            };
        }
    }

    fn forward3(
        &self,
        selector: Sel,
        webview: &WKWebView,
        navigation: Option<&WKNavigation>,
        error: &NSError,
    ) {
        if !self.inner_responds(selector) {
            return;
        }
        let inner = &*self.ivars().inner;
        unsafe {
            let _: () = if selector == sel!(webView:didFailNavigation:withError:) {
                msg_send![inner, webView: webview, didFailNavigation: navigation, withError: error]
            } else {
                msg_send![inner, webView: webview, didFailProvisionalNavigation: navigation, withError: error]
            };
        }
    }

    fn begin(&self, navigation: Option<&WKNavigation>) -> u64 {
        let ivars = self.ivars();
        let id = ivars.next_id.get() + 1;
        ivars.next_id.set(id);
        match navigation {
            Some(navigation) => {
                let mut list = ivars.navigations.borrow_mut();
                list.push((navigation.retain(), id));
                // Bounded: completed entries are removed; abandoned ones age out.
                if list.len() > 32 {
                    list.remove(0);
                }
            }
            None => ivars.anonymous.set(Some(id)),
        }
        id
    }

    fn lookup(&self, navigation: Option<&WKNavigation>) -> Option<u64> {
        let ivars = self.ivars();
        match navigation {
            Some(navigation) => ivars
                .navigations
                .borrow()
                .iter()
                .find(|(known, _)| std::ptr::eq(&**known, navigation))
                .map(|(_, id)| *id),
            None => ivars.anonymous.get(),
        }
    }

    fn complete(&self, webview: &WKWebView, navigation: Option<&WKNavigation>, success: bool) {
        let ivars = self.ivars();
        let id = match navigation {
            Some(navigation) => {
                let mut list = ivars.navigations.borrow_mut();
                let index = list
                    .iter()
                    .position(|(known, _)| std::ptr::eq(&**known, navigation));
                index.map(|index| list.remove(index).1)
            }
            None => ivars.anonymous.take(),
        };
        // A completion for a navigation we never saw start cannot be matched;
        // report a fresh, never-active ID so the shared Core rejects it.
        let id = id.unwrap_or_else(|| {
            let id = ivars.next_id.get() + 1;
            ivars.next_id.set(id);
            id
        });
        let url = webview_url(webview)
            .unwrap_or_else(|| url::Url::parse("about:blank").expect("static URL"));
        (ivars.on_finished)(id, url, success);
    }
}

/// Per-navigation identities equivalent to WebView2 NavigationId:
/// `on_started` fires on provisional start and redirects (same ID),
/// `on_finished` fires once on finish/fail/cancel.
pub fn install_navigation_listener<S, F>(
    webview: &Webview,
    tab_id: usize,
    on_started: S,
    on_finished: F,
) -> Result<(), String>
where
    S: Fn(u64, url::Url) + Send + 'static,
    F: Fn(u64, url::Url, bool) + Send + 'static,
{
    webview
        .with_webview(move |platform| unsafe {
            let Some(mtm) = MainThreadMarker::new() else {
                eprintln!("[gpt-wrapper] tab {tab_id}: navigation proxy needs the main thread");
                return;
            };
            let view = wk_webview(platform.inner());
            let Some(inner) = view.navigationDelegate() else {
                eprintln!("[gpt-wrapper] tab {tab_id}: WKWebView has no navigation delegate");
                return;
            };
            let inner: Retained<AnyObject> = Retained::cast_unchecked(inner);
            let proxy = NavigationProxy::new(mtm, inner, Box::new(on_started), Box::new(on_finished));
            view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*proxy)));
            // navigationDelegate is weak; tie the proxy's lifetime to the WebView.
            set_associated(view, &NAVIGATION_PROXY_KEY, &proxy);
        })
        .map_err(|error| format!("failed to access WKWebView for tab {tab_id}: {error}"))
}

/// `-[WKWebView stopLoading]`; an obsolete request then completes as failed.
pub fn stop_loading(webview: &Webview) -> Result<(), String> {
    webview
        .with_webview(|platform| unsafe { wk_webview(platform.inner()).stopLoading() })
        .map_err(|error| error.to_string())
}

// ---------------------------------------------------------------------------
// Media capture permission (forwarding WKUIDelegate proxy)
// ---------------------------------------------------------------------------

struct UiProxyIvars {
    inner: Retained<AnyObject>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "GptWrapperUIDelegateProxy"]
    #[ivars = UiProxyIvars]
    struct UiDelegateProxy;

    impl UiDelegateProxy {
        #[unsafe(method(respondsToSelector:))]
        fn responds_to_selector(&self, selector: Sel) -> bool {
            let own: bool = unsafe { msg_send![super(self), respondsToSelector: selector] };
            own || unsafe { msg_send![&*self.ivars().inner, respondsToSelector: selector] }
        }

        #[unsafe(method(forwardingTargetForSelector:))]
        fn forwarding_target(&self, selector: Sel) -> *mut AnyObject {
            let inner = &self.ivars().inner;
            let responds: bool = unsafe { msg_send![&**inner, respondsToSelector: selector] };
            if responds {
                Retained::as_ptr(inner) as *mut AnyObject
            } else {
                std::ptr::null_mut()
            }
        }
    }

    unsafe impl NSObjectProtocol for UiDelegateProxy {}

    unsafe impl WKUIDelegate for UiDelegateProxy {
        #[unsafe(method(webView:requestMediaCapturePermissionForOrigin:initiatedByFrame:type:decisionHandler:))]
        fn request_media_capture_permission(
            &self,
            webview: &WKWebView,
            origin: &WKSecurityOrigin,
            frame: &WKFrameInfo,
            capture_type: WKMediaCaptureType,
            decision_handler: &block2::DynBlock<dyn Fn(WKPermissionDecision)>,
        ) {
            let source = security_origin_source(origin);
            let main_frame = unsafe { frame.isMainFrame() };
            let decision = media_decision(
                registered_provider(webview),
                &source,
                main_frame,
                capture_type == WKMediaCaptureType::Microphone,
            );
            eprintln!(
                "[gpt-wrapper] media capture request origin={source} main_frame={main_frame} type={:?} -> {}",
                capture_type.0,
                if decision == WKPermissionDecision::Grant { "grant" } else { "deny" }
            );
            decision_handler.call((decision,));
        }
    }
);

/// Microphone only, only for the tab's own provider origin, only in the main
/// frame. Everything else is denied (camera / screen are outside v1 scope).
fn media_decision(
    tab: Option<(usize, Provider)>,
    source: &str,
    main_frame: bool,
    microphone_only: bool,
) -> WKPermissionDecision {
    match tab {
        Some((_, provider)) if main_frame && microphone_only && provider.trusts_source(source) => {
            WKPermissionDecision::Grant
        }
        _ => WKPermissionDecision::Deny,
    }
}

fn install_ui_delegate_proxy(view: &WKWebView, mtm: MainThreadMarker, tab_id: usize) {
    let Some(inner) = (unsafe { view.UIDelegate() }) else {
        eprintln!("[gpt-wrapper] tab {tab_id}: WKWebView has no UI delegate; media requests stay denied by WebKit");
        return;
    };
    let inner: Retained<AnyObject> = unsafe { Retained::cast_unchecked(inner) };
    let this = UiDelegateProxy::alloc(mtm).set_ivars(UiProxyIvars { inner });
    let proxy: Retained<UiDelegateProxy> = unsafe { msg_send![super(this), init] };
    unsafe {
        view.setUIDelegate(Some(ProtocolObject::from_ref(&*proxy)));
        set_associated(view, &UI_PROXY_KEY, &proxy);
    }
}

// ---------------------------------------------------------------------------
// Popups, link context menu and external browser
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkRoute {
    NewTab,
    Browser,
}

thread_local! {
    // tab ID -> explicit context-menu choice awaiting WebKit's new-window request.
    static PENDING_LINK_ROUTES: RefCell<HashMap<usize, (LinkRoute, Instant)>> =
        RefCell::new(HashMap::new());
    static SOUNDS: RefCell<Vec<Retained<NSSound>>> = const { RefCell::new(Vec::new()) };
}

const LINK_ROUTE_TTL: Duration = Duration::from_secs(3);

fn take_link_route(tab_id: usize) -> Option<LinkRoute> {
    PENDING_LINK_ROUTES.with(|routes| {
        let (route, at) = routes.borrow_mut().remove(&tab_id)?;
        (at.elapsed() <= LINK_ROUTE_TTL).then_some(route)
    })
}

/// Opens only http(s) URLs (validated by the caller) with the user's default
/// browser via LaunchServices, off the UI thread.
pub fn open_in_system_browser(url: url::Url, tab_id: usize) {
    tauri::async_runtime::spawn_blocking(move || {
        match std::process::Command::new("/usr/bin/open").arg(url.as_str()).status() {
            Ok(status) if status.success() => {}
            Ok(status) => eprintln!(
                "[gpt-wrapper] failed to open external link from tab {tab_id}: {url} (open exit {status})"
            ),
            Err(error) => eprintln!(
                "[gpt-wrapper] failed to open external link from tab {tab_id}: {url} ({error})"
            ),
        }
    });
}

/// WKWebView never opens its own windows: every `window.open`/`target=_blank`
/// goes to the default browser, except an explicit "open link in new tab"
/// context-menu choice, which becomes a GPT Wrapper tab of the same provider.
pub fn configure_tab_builder(
    builder: WebviewBuilder<Wry>,
    app: &AppHandle,
    tab_id: usize,
    provider: Provider,
) -> WebviewBuilder<Wry> {
    let app = app.clone();
    builder.on_new_window(move |url, _features| {
        match take_link_route(tab_id) {
            Some(LinkRoute::NewTab) => match link_tab_spec(provider, url.as_str()) {
                Ok(spec) => {
                    let worker_app = app.clone();
                    // add_child waits on the UI loop: never create tabs inside
                    // the WebKit delegate callback itself.
                    tauri::async_runtime::spawn_blocking(move || {
                        match create_tab_with_spec(&worker_app, provider, spec, false) {
                            Ok(_) => refresh_tab_host(&worker_app),
                            Err(error) => {
                                eprintln!("[gpt-wrapper] failed to open link in new tab: {error}")
                            }
                        }
                    });
                }
                Err(_) => eprintln!("[gpt-wrapper] ignored non-web link on tab {tab_id}: {url}"),
            },
            Some(LinkRoute::Browser) | None => match external_link_url(url.as_str()) {
                Ok(url) => open_in_system_browser(url, tab_id),
                Err(_) => eprintln!("[gpt-wrapper] ignored non-web popup URL on tab {tab_id}: {url}"),
            },
        }
        NewWindowResponse::Deny
    })
}

/// Popups are denied in `configure_tab_builder`; also install the media
/// permission policy, which needs the live WKWebView.
pub fn install_external_new_window_handler(webview: &Webview, tab_id: usize) -> Result<(), String> {
    let app = webview.app_handle().clone();
    webview
        .with_webview(move |platform| unsafe {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let view = wk_webview(platform.inner());
            register_tab(&app, view, tab_id);
            install_ui_delegate_proxy(view, mtm, tab_id);
        })
        .map_err(|error| format!("failed to access WKWebView on tab {tab_id}: {error}"))
}

struct LinkMenuTargetIvars;

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "GptWrapperLinkMenuTarget"]
    #[ivars = LinkMenuTargetIvars]
    struct LinkMenuTarget;

    impl LinkMenuTarget {
        #[unsafe(method(gptWrapperOpenLinkInNewTab:))]
        fn open_in_new_tab(&self, sender: &NSMenuItem) {
            route_link_menu_item(sender, LinkRoute::NewTab);
        }

        #[unsafe(method(gptWrapperOpenLinkInBrowser:))]
        fn open_in_browser(&self, sender: &NSMenuItem) {
            route_link_menu_item(sender, LinkRoute::Browser);
        }
    }

    unsafe impl NSObjectProtocol for LinkMenuTarget {}
);

thread_local! {
    static LINK_MENU_TARGET: RefCell<Option<Retained<LinkMenuTarget>>> = const { RefCell::new(None) };
}

fn link_menu_target(mtm: MainThreadMarker) -> Retained<LinkMenuTarget> {
    LINK_MENU_TARGET.with(|cell| {
        cell.borrow_mut()
            .get_or_insert_with(|| {
                let this = LinkMenuTarget::alloc(mtm).set_ivars(LinkMenuTargetIvars);
                unsafe { msg_send![super(this), init] }
            })
            .clone()
    })
}

// Our item carries the tab ID (tag) and WebKit's original "Open Link in New
// Window" item (representedObject). Performing WebKit's own action makes
// WebKit hand us the exact link URL through the UI delegate; the recorded
// route then decides between a GPT Wrapper tab and the default browser.
fn route_link_menu_item(sender: &NSMenuItem, route: LinkRoute) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let tab_id = sender.tag() as usize;
    let Some(original) = sender.representedObject() else {
        return;
    };
    let Some(original) = original.downcast_ref::<NSMenuItem>() else {
        return;
    };
    let Some(action) = original.action() else {
        return;
    };
    PENDING_LINK_ROUTES.with(|routes| {
        routes.borrow_mut().insert(tab_id, (route, Instant::now()));
    });
    let target = original.target();
    let sent = unsafe {
        NSApplication::sharedApplication(mtm).sendAction_to_from(
            action,
            target.as_deref(),
            Some(original),
        )
    };
    if !sent {
        PENDING_LINK_ROUTES.with(|routes| routes.borrow_mut().remove(&tab_id));
        eprintln!("[gpt-wrapper] link context menu action could not be delivered on tab {tab_id}");
    }
}

const WEBKIT_OPEN_LINK_IN_NEW_WINDOW: &str = "WKMenuItemIdentifierOpenLinkInNewWindow";
const WEBKIT_DOWNLOAD_LINKED_FILE: &str = "WKMenuItemIdentifierDownloadLinkedFile";

fn menu_item_identifier(item: &NSMenuItem) -> Option<String> {
    let identifier: Option<Retained<NSString>> = unsafe { msg_send![item, identifier] };
    identifier.map(|identifier| identifier.to_string())
}

fn new_menu_item(
    mtm: MainThreadMarker,
    title: &str,
    action: Sel,
    target: &AnyObject,
    tag: isize,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(""),
        )
    };
    unsafe { item.setTarget(Some(target)) };
    item.setTag(tag);
    item
}

unsafe extern "C-unwind" fn will_open_menu(this: &AnyObject, cmd: Sel, menu: &NSMenu, event: &NSEvent) {
    // WKWebView's own implementation first.
    let _: () = msg_send![super(this, WKWebView::class()), willOpenMenu: menu, withEvent: event];
    let _ = cmd;
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(tab_id) = tab_id_of(this) else {
        return; // Tab bar / Settings / notification WebViews keep WebKit's menu.
    };
    let items = menu.itemArray();
    let mut open_link = None;
    let mut downloads = Vec::new();
    for index in 0..items.count() {
        let item = items.objectAtIndex(index);
        match menu_item_identifier(&item).as_deref() {
            Some(WEBKIT_OPEN_LINK_IN_NEW_WINDOW) => open_link = Some((index, item)),
            // wry cancels downloads without a handler; don't offer a dead item.
            Some(WEBKIT_DOWNLOAD_LINKED_FILE) => downloads.push(item),
            _ => {}
        }
    }
    let Some((index, original)) = open_link else {
        return; // Not a link: leave the native menu unchanged.
    };
    let target = link_menu_target(mtm);
    let tab_item = new_menu_item(
        mtm,
        "在新分頁中開啟連結",
        sel!(gptWrapperOpenLinkInNewTab:),
        &target,
        tab_id as isize,
    );
    let browser_item = new_menu_item(
        mtm,
        "在預設瀏覽器開啟連結",
        sel!(gptWrapperOpenLinkInBrowser:),
        &target,
        tab_id as isize,
    );
    tab_item.setRepresentedObject(Some(&original));
    browser_item.setRepresentedObject(Some(&original));
    menu.removeItem(&original);
    for item in downloads {
        menu.removeItem(&item);
    }
    let index = index.min(menu.numberOfItems() as usize) as isize;
    menu.insertItem_atIndex(&browser_item, index);
    menu.insertItem_atIndex(&tab_item, index);
}

fn install_will_open_menu_hook(view: &WKWebView) {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    INSTALLED.get_or_init(|| unsafe {
        let class = view.class();
        let selector = sel!(willOpenMenu:withEvent:);
        let imp: unsafe extern "C-unwind" fn(&AnyObject, Sel, &NSMenu, &NSEvent) = will_open_menu;
        let added = objc2::ffi::class_addMethod(
            class as *const AnyClass as *mut AnyClass,
            selector,
            std::mem::transmute::<
                unsafe extern "C-unwind" fn(&AnyObject, Sel, &NSMenu, &NSEvent),
                objc2::runtime::Imp,
            >(imp),
            c"v@:@@".as_ptr(),
        );
        if !added.as_bool() {
            eprintln!(
                "[gpt-wrapper] {} already overrides willOpenMenu:withEvent:; link menu unchanged",
                class.name().to_string_lossy()
            );
        }
        added.as_bool()
    });
}

/// Replaces WebKit's "Open Link in New Window" with two explicit choices:
/// "在新分頁中開啟連結" and "在預設瀏覽器開啟連結". Other menus are untouched.
pub fn install_link_context_menu(
    app: &AppHandle,
    webview: &Webview,
    tab_id: usize,
    _provider: Provider,
) -> Result<(), String> {
    let app = app.clone();
    webview
        .with_webview(move |platform| unsafe {
            let view = wk_webview(platform.inner());
            register_tab(&app, view, tab_id);
            install_will_open_menu_hook(view);
        })
        .map_err(|error| format!("failed to access WKWebView on tab {tab_id}: {error}"))
}

// ---------------------------------------------------------------------------
// Project bookmark popup menu (press - drag - release)
// ---------------------------------------------------------------------------

struct BookmarkMenuTargetIvars {
    selected: Cell<Option<isize>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "GptWrapperBookmarkMenuTarget"]
    #[ivars = BookmarkMenuTargetIvars]
    struct BookmarkMenuTarget;

    impl BookmarkMenuTarget {
        #[unsafe(method(gptWrapperSelectBookmark:))]
        fn select(&self, sender: &NSMenuItem) {
            self.ivars().selected.set(Some(sender.tag()));
        }
    }

    unsafe impl NSObjectProtocol for BookmarkMenuTarget {}
);

fn left_mouse_down() -> bool {
    NSEvent::pressedMouseButtons() & 1 != 0
}

/// Runs on the UI thread. Same contract as the Windows TrackPopupMenuEx path:
/// None when the button was already released or nothing was chosen; nil UUID
/// for the built-in home; the menu's right edge is aligned to the anchor.
pub fn track_project_bookmark_menu(
    app: &AppHandle,
    provider: Provider,
    settings: ProjectList,
    anchor: MenuAnchor,
) -> Result<Option<Uuid>, String> {
    let mtm = MainThreadMarker::new().ok_or("bookmark menu must run on the main thread")?;
    // IPC can reach the UI thread after the mouse has already been released.
    if !left_mouse_down() {
        return Ok(None);
    }
    let window = app
        .get_window(MAIN_WINDOW_LABEL)
        .ok_or("main window is missing")?;
    let content_view = window.ns_view().map_err(|error| error.to_string())?;
    let content_view = unsafe { &*(content_view as *const NSView) };

    let target = {
        let this = BookmarkMenuTarget::alloc(mtm).set_ivars(BookmarkMenuTargetIvars {
            selected: Cell::new(None),
        });
        let target: Retained<BookmarkMenuTarget> = unsafe { msg_send![super(this), init] };
        target
    };
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(""));
    menu.setAutoenablesItems(false);
    let add = |title: &str, tag: isize, checked: bool| {
        let item = new_menu_item(mtm, title, sel!(gptWrapperSelectBookmark:), &target, tag);
        item.setEnabled(true);
        if checked {
            let _: () = unsafe { msg_send![&*item, setState: 1isize] };
        }
        menu.addItem(&item);
    };
    add(
        background_project_switcher::builtin_label(provider),
        1,
        settings.default_project_id.is_none(),
    );
    for (index, project) in settings.projects.iter().enumerate() {
        add(
            &project.name.replace('\0', ""),
            index as isize + 2,
            settings.default_project_id == Some(project.id),
        );
    }

    if !left_mouse_down() {
        return Ok(None);
    }
    let width = menu.size().width;
    let height = content_view.bounds().size.height;
    let x = (anchor.right - width).max(0.0);
    let y = if content_view.isFlipped() {
        anchor.bottom
    } else {
        height - anchor.bottom
    };
    menu.popUpMenuPositioningItem_atLocation_inView(None, NSPoint::new(x, y), Some(content_view));
    let command = target.ivars().selected.get().unwrap_or(0);
    Ok(if command == 1 {
        Some(Uuid::nil())
    } else {
        usize::try_from(command)
            .ok()
            .and_then(|command| command.checked_sub(2))
            .and_then(|index| settings.projects.get(index).map(|p| p.id))
    })
}

// ---------------------------------------------------------------------------
// Window focus and non-activating overlays
// ---------------------------------------------------------------------------

/// Unhides the app, de-miniaturizes and focuses the window. Zoomed/full-screen
/// state is left exactly as the user had it (no implicit maximize/restore).
pub fn restore_to_foreground(window: &Window) {
    if let Err(error) = window.app_handle().show() {
        eprintln!("[gpt-wrapper] failed to unhide app: {error:?}");
    }
    let _ = window.show();
    if window.is_minimized().unwrap_or(false) {
        let _ = window.unminimize();
    }
    let _ = window.set_focus();
}

/// Tauri's default macOS title-bar style (`Visible`) also enables
/// `NSFullSizeContentView`, so the content view extends *under* the title bar.
/// The main window stacks child WebViews itself (tab bar at y = 0), so that
/// would hide the tab bar behind the title bar whenever the window is not
/// fullscreen. Keep the content area below the title bar instead. Idempotent;
/// call before adding children and again on resize, because leaving native
/// fullscreen can restore the original style mask. Returns true if changed.
pub fn keep_content_below_title_bar(window: &Window) -> bool {
    if MainThreadMarker::new().is_none() {
        return false;
    }
    let Ok(pointer) = window.ns_window() else {
        return false;
    };
    if pointer.is_null() {
        return false;
    }
    let ns_window = unsafe { &*(pointer as *const NSWindow) };
    let mask = ns_window.styleMask();
    if !mask.contains(NSWindowStyleMask::FullSizeContentView)
        || mask.contains(NSWindowStyleMask::FullScreen)
    {
        return false;
    }
    ns_window.setStyleMask(mask & !NSWindowStyleMask::FullSizeContentView);
    true
}

fn ns_window_of(window: &WebviewWindow) -> Result<&NSWindow, String> {
    if MainThreadMarker::new().is_none() {
        return Err("overlay windows must be changed on the main thread".into());
    }
    let pointer = window.ns_window().map_err(|error| error.to_string())?;
    if pointer.is_null() {
        return Err("NSWindow unavailable".into());
    }
    Ok(unsafe { &*(pointer as *const NSWindow) })
}

pub fn hide_overlay(window: &WebviewWindow) -> Result<(), String> {
    let ns_window = ns_window_of(window)?;
    ns_window.orderOut(None);
    if ns_window.isVisible() {
        let _ = window.hide();
        ns_window.orderOut(None);
    }
    if ns_window.isVisible() {
        Err("overlay NSWindow remained visible after orderOut".into())
    } else {
        Ok(())
    }
}

/// Shows an overlay above other apps without activating GPT Wrapper or making
/// the overlay key. `interactive` overlays (with a close button) become a
/// non-activating panel so clicking them does not steal focus either.
pub fn show_overlay_without_activation(
    window: &WebviewWindow,
    interactive: bool,
) -> Result<(), String> {
    let ns_window = ns_window_of(window)?;
    if interactive {
        convert_to_nonactivating_panel(ns_window);
    }
    // kCGStatusWindowLevel: above normal and floating windows, below menus.
    ns_window.setLevel(25);
    ns_window.setHidesOnDeactivate(false);
    ns_window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    ns_window.orderFrontRegardless();
    if ns_window.isVisible() {
        Ok(())
    } else {
        Err("overlay NSWindow not visible after orderFrontRegardless".into())
    }
}

/// Clear NSWindow + WKWebView background so only the HTML cards are drawn.
pub fn make_overlay_transparent(window: &WebviewWindow) -> Result<(), String> {
    let ns_window = ns_window_of(window)?;
    ns_window.setOpaque(false);
    ns_window.setBackgroundColor(Some(&objc2_app_kit::NSColor::clearColor()));
    window
        .with_webview(|platform| unsafe {
            let view = wk_webview(platform.inner());
            let no = NSNumber::new_bool(false);
            let _: () = msg_send![view, setValue: &*no, forKey: &*NSString::from_str("drawsBackground")];
        })
        .map_err(|error| error.to_string())
}

/// Debug harness only: runs the installed `willOpenMenu:withEvent:` hook on a
/// tab's real WKWebView with a menu shaped like WebKit's link menu (public
/// WKMenuItemIdentifier* identifiers) and returns the resulting titles.
#[cfg(debug_assertions)]
pub fn exercise_link_menu_hook(webview: &Webview) -> String {
    let (sender, receiver) = std::sync::mpsc::channel();
    let result = webview.with_webview(move |platform| unsafe {
        let Some(mtm) = MainThreadMarker::new() else {
            let _ = sender.send("ERR not main thread".to_string());
            return;
        };
        let view = wk_webview(platform.inner());
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(""));
        for (title, identifier) in [
            ("Open Link", "WKMenuItemIdentifierOpenLink"),
            ("Open Link in New Window", WEBKIT_OPEN_LINK_IN_NEW_WINDOW),
            ("Download Linked File", WEBKIT_DOWNLOAD_LINKED_FILE),
            ("Copy Link", "WKMenuItemIdentifierCopyLink"),
        ] {
            let item = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(sel!(copy:)),
                &NSString::from_str(""),
            );
            let _: () = msg_send![&*item, setIdentifier: &*NSString::from_str(identifier)];
            menu.addItem(&item);
        }
        let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            NSEventType::RightMouseDown, NSPoint::new(10.0, 10.0), NSEventModifierFlags::empty(),
            0.0, 0, None, 0, 1, 1.0,
        );
        let Some(event) = event else {
            let _ = sender.send("ERR event".to_string());
            return;
        };
        let _: () = msg_send![view, willOpenMenu: &*menu, withEvent: &*event];
        let items = menu.itemArray();
        let titles: Vec<String> = (0..items.count())
            .map(|index| {
                let item = items.objectAtIndex(index);
                format!("{}{}", item.title(), if item.representedObject().is_some() { "[routes:WebKit item]" } else { "" })
            })
            .collect();
        let _ = sender.send(titles.join(" | "));
    });
    if let Err(error) = result {
        return format!("ERR {error}");
    }
    receiver.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|_| "ERR timeout".into())
}

/// Debug harness only: records a pending context-menu route exactly as the
/// menu item action does, without needing a physical right-click.
#[cfg(debug_assertions)]
pub fn set_pending_link_route_for_test(tab_id: usize, new_tab: bool) {
    let route = if new_tab { LinkRoute::NewTab } else { LinkRoute::Browser };
    PENDING_LINK_ROUTES.with(|routes| {
        routes.borrow_mut().insert(tab_id, (route, Instant::now()));
    });
}

/// Debug harness only: window / content / title-bar-safe rects and each
/// child WebView's frame in window coordinates (bottom-left origin).
#[cfg(debug_assertions)]
pub fn describe_layout(window: &Window) -> String {
    let Ok(pointer) = window.ns_window() else { return "ERR ns_window".into() };
    let ns_window = unsafe { &*(pointer as *const NSWindow) };
    let r = |rect: objc2_foundation::NSRect| format!("({:.0},{:.0} {:.0}x{:.0})", rect.origin.x, rect.origin.y, rect.size.width, rect.size.height);
    let mut out = format!(
        "frame={} contentLayoutRect={} fullSizeContent={} fullscreen={}",
        r(ns_window.frame()),
        r(ns_window.contentLayoutRect()),
        ns_window.styleMask().contains(NSWindowStyleMask::FullSizeContentView),
        ns_window.styleMask().contains(NSWindowStyleMask::FullScreen),
    );
    if let Some(content) = ns_window.contentView() {
        out += &format!(" contentView={} flipped={}", r(content.frame()), content.isFlipped());
        for sub in content.subviews().iter() {
            let in_window = sub.convertRect_toView(sub.bounds(), None);
            out += &format!(" | {}={} super={}", sub.class().name().to_string_lossy(), r(in_window), r(content.frame()));
        }
    }
    out += &format!(" | inner_size={:?} scale={:?}", window.inner_size().ok(), window.scale_factor().ok());
    out
}

/// Debug harness only: class / style / key-ability / level of a window.
#[cfg(debug_assertions)]
pub fn describe_window(window: &WebviewWindow) -> String {
    match ns_window_of(window) {
        Ok(ns_window) => format!(
            "class={} nonactivating={} canBecomeKey={} visible={} level={} key={}",
            ns_window.class().name().to_string_lossy(),
            ns_window.styleMask().contains(NSWindowStyleMask::NonactivatingPanel),
            ns_window.canBecomeKeyWindow(),
            ns_window.isVisible(),
            ns_window.level(),
            ns_window.isKeyWindow(),
        ),
        Err(error) => format!("ERR {error}"),
    }
}

extern "C-unwind" fn panel_cannot_become_key(_this: &AnyObject, _cmd: Sel) -> Bool {
    Bool::NO
}

fn nonactivating_panel_class() -> Option<&'static AnyClass> {
    static CLASS: OnceLock<Option<usize>> = OnceLock::new();
    CLASS
        .get_or_init(|| {
            let mut builder = ClassBuilder::new(c"GptWrapperNonactivatingPanel", NSPanel::class())?;
            // Same ivar name/type as tao's TaoWindow so tao keeps working.
            builder.add_ivar::<Bool>(c"focusable");
            unsafe {
                builder.add_method(
                    sel!(canBecomeKeyWindow),
                    panel_cannot_become_key as extern "C-unwind" fn(_, _) -> _,
                );
                builder.add_method(
                    sel!(canBecomeMainWindow),
                    panel_cannot_become_key as extern "C-unwind" fn(_, _) -> _,
                );
            }
            Some(builder.register() as *const AnyClass as usize)
        })
        .map(|class| unsafe { &*(class as *const AnyClass) })
}

fn ivar_offset(class: &AnyClass, name: &CStr) -> Option<isize> {
    let ivar = unsafe { objc2::ffi::class_getInstanceVariable(class, name.as_ptr()) };
    (!ivar.is_null()).then(|| unsafe { objc2::ffi::ivar_getOffset(ivar) })
}

// Clicking a normal NSWindow of an inactive app activates the app. Only an
// NSPanel with NSWindowStyleMaskNonactivatingPanel avoids that. tao creates an
// NSWindow subclass, so the instance is re-classed to an NSPanel subclass, but
// only when the memory layout provably matches; otherwise the overlay still
// appears without activation and only a click on it may activate the app.
fn convert_to_nonactivating_panel(ns_window: &NSWindow) {
    let current = ns_window.class();
    if current.name() == c"GptWrapperNonactivatingPanel" {
        return;
    }
    let Some(panel) = nonactivating_panel_class() else {
        eprintln!("[gpt-wrapper] non-activating panel class unavailable");
        return;
    };
    let same_layout = unsafe {
        objc2::ffi::class_getInstanceSize(panel) <= objc2::ffi::class_getInstanceSize(current)
    } && ivar_offset(panel, c"focusable") == ivar_offset(current, c"focusable");
    if !same_layout {
        eprintln!(
            "[gpt-wrapper] overlay stays a regular window: {} layout differs from NSPanel",
            current.name().to_string_lossy()
        );
        return;
    }
    unsafe {
        objc2::ffi::object_setClass(
            ns_window as *const NSWindow as *mut AnyObject,
            panel as *const AnyClass,
        );
    }
    ns_window.setStyleMask(ns_window.styleMask() | NSWindowStyleMask::NonactivatingPanel);
    let _: () = unsafe { msg_send![ns_window, setBecomesKeyOnlyIfNeeded: true] };
}

/// Plays a bundled sound file through AppKit (no extra permission needed).
pub fn play_sound_file(path: &std::path::Path) -> Result<(), String> {
    MainThreadMarker::new().ok_or("sound must be started on the main thread")?;
    let path = path.to_str().ok_or("sound path is not UTF-8")?;
    let sound = NSSound::initWithContentsOfFile_byReference(
        NSSound::alloc(),
        &NSString::from_str(path),
        true,
    )
    .ok_or("NSSound could not load the completion sound")?;
    if !sound.play() {
        return Err("NSSound rejected the completion sound".into());
    }
    // NSSound must stay alive while playing; keep the most recent few.
    SOUNDS.with(|sounds| {
        let mut sounds = sounds.borrow_mut();
        sounds.retain(|sound| sound.isPlaying());
        sounds.push(sound);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_capture_is_microphone_only_for_own_provider_main_frame() {
        let chatgpt = Some((1, Provider::Chatgpt));
        let claude = Some((2, Provider::Claude));
        assert_eq!(media_decision(chatgpt, "https://chatgpt.com/", true, true), WKPermissionDecision::Grant);
        assert_eq!(media_decision(claude, "https://claude.ai/", true, true), WKPermissionDecision::Grant);
        // Wrong provider, iframe, camera, unknown tab, non-HTTPS: denied.
        assert_eq!(media_decision(chatgpt, "https://claude.ai/", true, true), WKPermissionDecision::Deny);
        assert_eq!(media_decision(claude, "https://claude.ai/", false, true), WKPermissionDecision::Deny);
        assert_eq!(media_decision(claude, "https://claude.ai/", true, false), WKPermissionDecision::Deny);
        assert_eq!(media_decision(None, "https://claude.ai/", true, true), WKPermissionDecision::Deny);
        assert_eq!(media_decision(chatgpt, "http://chatgpt.com/", true, true), WKPermissionDecision::Deny);
        assert_eq!(media_decision(claude, "https://claude.ai:8443/", true, true), WKPermissionDecision::Deny);
    }

    #[test]
    fn link_routes_expire_and_are_consumed_once() {
        PENDING_LINK_ROUTES.with(|routes| {
            routes.borrow_mut().insert(7, (LinkRoute::NewTab, Instant::now()));
            routes
                .borrow_mut()
                .insert(8, (LinkRoute::Browser, Instant::now() - LINK_ROUTE_TTL * 2));
        });
        assert_eq!(take_link_route(7), Some(LinkRoute::NewTab));
        assert_eq!(take_link_route(7), None);
        assert_eq!(take_link_route(8), None);
    }
}

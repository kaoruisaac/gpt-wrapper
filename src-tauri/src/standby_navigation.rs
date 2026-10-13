//! Page identity for a reused standby, retained when it becomes a visible PTT tab.
use std::time::{Duration, Instant};
use url::Url;

pub const DEBOUNCE: Duration = Duration::from_millis(300);

#[derive(Debug)]
pub struct Navigation {
    pub desired: Url,
    issued: Url,
    loaded: Option<Url>,
    active: Option<u64>,
    last_id: u64,
    awaiting: bool,
    pub failed: bool,
}

fn same_page(a: &Url, b: &Url) -> bool {
    let chatgpt = |u: &Url| {
        u.scheme() == "https" && matches!(u.host_str(), Some("chatgpt.com" | "chat.openai.com"))
    };
    (a.origin() == b.origin() || (chatgpt(a) && chatgpt(b)))
        && a.path().trim_end_matches('/') == b.path().trim_end_matches('/')
        && a.query() == b.query()
        && a.fragment() == b.fragment()
}

impl Navigation {
    pub fn new(url: Url) -> Self {
        Self {
            desired: url.clone(),
            issued: url,
            loaded: None,
            active: None,
            last_id: 0,
            awaiting: true,
            failed: false,
        }
    }

    pub fn ready(&self) -> bool {
        !self.failed
            && !self.awaiting
            && self.active.is_none()
            && self
                .loaded
                .as_ref()
                .is_some_and(|url| same_page(url, &self.desired))
    }

    // Returns whether the caller must navigate. An in-flight request can be reused,
    // but a previous request's URL alone is never evidence of a loaded page.
    pub fn prepare(&mut self, target: Url) -> bool {
        self.desired = target.clone();
        if self.ready()
            || (!self.failed && self.issued == target && (self.awaiting || self.active.is_some()))
        {
            return false;
        }
        self.issued = target;
        self.loaded = None;
        self.active = None;
        self.awaiting = true;
        self.failed = false;
        true
    }

    pub fn started(&mut self, id: u64, url: &Url) -> bool {
        if self.active == Some(id) {
            return true;
        } // redirect in the same navigation
        let fresh = id > self.last_id;
        self.last_id = self.last_id.max(id);
        if fresh && same_page(url, &self.issued) {
            self.active = Some(id);
            self.awaiting = false;
            self.loaded = None;
            self.failed = false;
            true
        } else {
            if fresh {
                // A login/error/unrequested page must never inherit Ready from
                // the previous document. A later correct start can recover.
                self.fail();
            }
            false
        }
    }

    pub fn finished(&mut self, id: u64, url: &Url, success: bool) -> bool {
        if self.active != Some(id) {
            return false;
        }
        self.active = None;
        self.failed = !success || !same_page(url, &self.issued);
        self.loaded = (!self.failed).then(|| url.clone());
        self.ready()
    }

    pub fn fail(&mut self) {
        self.failed = true;
        self.awaiting = false;
        self.active = None;
        self.loaded = None;
    }
}

#[derive(Debug, Default)]
pub struct Debounce {
    pub generation: u64,
    pub pending: Option<(usize, Instant)>,
    pub worker_running: bool,
}

impl Debounce {
    pub fn schedule(&mut self, id: usize, now: Instant) -> bool {
        self.generation = self.generation.wrapping_add(1);
        self.pending = Some((id, now + DEBOUNCE));
        let start_worker = !self.worker_running;
        self.worker_running = true;
        start_worker
    }
    pub fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.pending = None;
    }
    pub fn due(&self, generation: u64, id: usize, now: Instant) -> bool {
        self.generation == generation
            && self
                .pending
                .is_some_and(|(target, deadline)| target == id && now >= deadline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn url(name: &str) -> Url {
        Url::parse(&format!("https://chatgpt.com/g/{name}/project")).unwrap()
    }
    fn loaded(name: &str) -> Navigation {
        let mut n = Navigation::new(url(name));
        assert!(n.started(1, &url(name)));
        assert!(n.finished(1, &url(name), true));
        n
    }

    #[test]
    fn rapid_changes_execute_only_last_target_with_same_identity() {
        let mut d = Debounce::default();
        let mut n = loaded("a");
        let start = Instant::now();
        let id = 7;
        let mut calls = Vec::new();
        for (i, name) in ["a", "b", "c"].into_iter().enumerate() {
            let now = start + Duration::from_millis(i as u64 * 100);
            n.desired = url(name);
            assert_eq!(d.schedule(id, now), i == 0);
            assert!(!d.due(d.generation, id, now));
        }
        assert!(!d.due(1, id, start + Duration::from_secs(1)));
        assert!(!d.due(d.generation, id, start + Duration::from_millis(499)));
        assert!(d.due(d.generation, id, start + Duration::from_millis(500)));
        if n.prepare(n.desired.clone()) {
            calls.push((id, n.desired.clone()));
        }
        assert_eq!(calls, vec![(id, url("c"))]);
        assert!(!n.ready());
    }

    #[test]
    fn loaded_same_url_and_return_before_debounce_do_not_navigate() {
        let mut n = loaded("a");
        assert!(!n.prepare(url("a")));
        n.desired = url("b");
        assert!(!n.ready());
        assert!(!n.prepare(url("a")));
        assert!(n.ready());
    }

    #[test]
    fn late_completion_cannot_make_a_new_target_ready() {
        let mut n = loaded("a");
        assert!(n.prepare(url("b")));
        assert!(n.started(2, &url("b")));
        n.desired = url("c");
        assert!(!n.finished(2, &url("b"), true));
        assert!(!n.ready());
        assert!(n.prepare(url("c")));
        assert!(n.started(3, &url("c")));
        assert!(!n.finished(2, &url("b"), true));
        assert!(n.finished(3, &url("c"), true));
        assert!(!n.finished(3, &url("c"), true));
    }

    #[test]
    fn a_b_a_and_same_url_late_events_require_current_navigation_id() {
        let mut n = loaded("a");
        assert!(n.prepare(url("b")));
        assert!(n.started(2, &url("b")));
        assert!(n.prepare(url("a")));
        assert!(!n.started(1, &url("a")));
        assert!(!n.finished(1, &url("a"), true));
        assert!(!n.finished(2, &url("b"), true));
        assert!(!n.ready());
        assert!(n.started(3, &url("a")));
        assert!(!n.finished(1, &url("a"), true));
        assert!(n.finished(3, &url("a"), true));
    }

    #[test]
    fn ptt_preempts_deadline_and_promotion_invalidates_timer() {
        let mut n = loaded("a");
        let mut d = Debounce::default();
        let now = Instant::now();
        n.desired = url("b");
        d.schedule(7, now);
        let old = d.generation;
        d.cancel();
        assert!(n.prepare(url("b"))); // immediate PTT navigation
        assert!(!n.ready());
        assert!(!d.due(old, 7, now + DEBOUNCE));
        assert!(n.started(2, &url("b")));
        assert!(!n.prepare(url("b"))); // reuse the correct in-flight request
        assert!(n.finished(2, &url("b"), true));
    }

    #[test]
    fn missing_or_replaced_standby_and_cancel_are_isolated() {
        let mut d = Debounce::default();
        let now = Instant::now();
        d.schedule(7, now);
        let old = d.generation;
        assert!(!d.due(old, 8, now + DEBOUNCE));
        d.cancel();
        d.schedule(8, now);
        assert!(!d.due(old, 7, now + DEBOUNCE));
        assert!(d.due(d.generation, 8, now + DEBOUNCE));
    }

    #[test]
    fn failed_navigation_or_wrong_redirect_never_ready_and_can_retry() {
        let mut n = Navigation::new(url("a"));
        assert!(n.started(1, &url("a")));
        assert!(!n.finished(1, &url("a"), false));
        assert!(n.failed);
        assert!(n.prepare(url("a")));
        assert!(n.started(2, &url("a")));
        assert!(n.started(2, &url("b"))); // redirect belongs to the same ID
        assert!(!n.finished(2, &url("b"), true));
        assert!(!n.ready());
        n.fail();
        assert!(n.prepare(url("a")));
    }

    #[test]
    fn canonical_chatgpt_alias_redirect_is_accepted() {
        let original = Url::parse("https://chat.openai.com/").unwrap();
        let canonical = Url::parse("https://chatgpt.com/").unwrap();
        let mut n = Navigation::new(original);
        assert!(n.started(1, &canonical));
        assert!(n.finished(1, &canonical, true));
        assert!(!n.prepare(canonical));
    }

    #[test]
    fn each_switch_resets_trailing_deadline() {
        let mut d = Debounce::default();
        let now = Instant::now();
        d.schedule(7, now);
        let old = d.generation;
        d.schedule(7, now + Duration::from_millis(299));
        assert!(!d.due(old, 7, now + DEBOUNCE));
        assert!(!d.due(d.generation, 7, now + Duration::from_millis(598)));
        assert!(d.due(d.generation, 7, now + Duration::from_millis(599)));
    }

    #[test]
    fn unexpected_document_invalidates_loaded_readiness() {
        let mut n = loaded("a");
        assert!(!n.started(2, &Url::parse("https://chatgpt.com/auth/login").unwrap()));
        assert!(!n.ready());
        assert!(!n.finished(2, &url("a"), true));
        assert!(n.prepare(url("a")));
        assert!(n.started(3, &url("a")));
        assert!(n.finished(3, &url("a"), true));
    }

    #[test]
    fn cancel_and_reschedule_share_the_existing_worker() {
        let mut d = Debounce::default();
        let now = Instant::now();
        assert!(d.schedule(7, now));
        d.cancel();
        assert!(!d.schedule(8, now));
        // Only the worker/UI callback releases ownership when no work remains.
        d.cancel();
        d.worker_running = false;
        assert!(d.schedule(9, now));
    }
}

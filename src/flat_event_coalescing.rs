//! Bounded scene-work requests: camera deltas are never queued or reordered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub hit_test: bool,
    pub preserve_declutter: bool,
}
#[derive(Default, Debug)]
pub(crate) struct Pending {
    enabled: bool,
    request: Option<Request>,
    retry_armed: bool,
}
impl Pending {
    pub fn new(value: Option<&std::ffi::OsStr>) -> Self {
        Self {
            enabled: value.is_none() || value.and_then(|v| v.to_str()) == Some("1"),
            ..Self::default()
        }
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn may_defer(&self, loaded_plugins: bool) -> bool {
        self.enabled && !loaded_plugins
    }
    pub fn dirty(&self) -> bool {
        self.request.is_some()
    }
    pub fn request(&mut self, hit_test: bool, preserve_declutter: bool) {
        if !self.enabled {
            return;
        }
        self.request = Some(match self.request {
            Some(old) => Request {
                hit_test: old.hit_test || hit_test,
                preserve_declutter: old.preserve_declutter && preserve_declutter,
            },
            None => Request {
                hit_test,
                preserve_declutter,
            },
        });
        self.retry_armed = true;
    }
    pub fn requirements(&self) -> Option<Request> {
        self.request
    }
    pub fn runnable(&self) -> Option<Request> {
        if self.retry_armed {
            self.request
        } else {
            None
        }
    }
    pub fn finish(&mut self, ready: bool) {
        if ready {
            self.request = None;
        }
        self.retry_armed = false;
    }
    pub fn explicit_retry(&mut self) {
        if self.dirty() {
            self.retry_armed = true;
        }
    }
    /// A redraw alone is not a consumer request. Preserve failed-flush disarm
    /// unless loaded plugin callbacks or actual queued UI events need the scene.
    pub fn plugin_ui_consumer(&mut self, loaded_plugins: bool, queued_ui_events: bool) -> bool {
        let consumer = loaded_plugins || queued_ui_events;
        if consumer {
            self.explicit_retry();
        }
        consumer
    }
    /// Complete authoritative publication or clear, not merely an attempted flush.
    pub fn authoritative_ready(&mut self) {
        self.request = None;
        self.retry_armed = false;
    }
}
/// Private App runtime seam: production and fault-injected App unit controls
/// execute this same dispatcher. No geometry/camera calculation is replaced.
pub(crate) trait Runtime {
    fn scene_pending(&self) -> &Pending;
    fn scene_pending_mut(&mut self) -> &mut Pending;
    fn begin_scene_flush(&mut self);
    fn rebuild_scene(&mut self, request: Request);
    fn end_scene_flush(&mut self);
}
pub(crate) fn flush_runtime(runtime: &mut impl Runtime) -> bool {
    if !runtime.scene_pending().dirty() {
        return true;
    }
    let Some(request) = runtime.scene_pending().runnable() else {
        return false;
    };
    runtime.begin_scene_flush();
    runtime.rebuild_scene(request);
    runtime.end_scene_flush();
    !runtime.scene_pending().dirty()
}
pub(crate) fn ensure_runtime(runtime: &mut impl Runtime) -> bool {
    runtime.scene_pending_mut().explicit_retry();
    flush_runtime(runtime)
}

/// Zero-plugin ordinary redraw is not an explicit retry consumer.
pub(crate) fn plugin_ui_runtime(
    runtime: &mut impl Runtime,
    loaded_plugins: bool,
    queued_ui_events: bool,
) -> bool {
    if runtime
        .scene_pending_mut()
        .plugin_ui_consumer(loaded_plugins, queued_ui_events)
    {
        ensure_runtime(runtime)
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_enabled_and_explicit_policy_and_nonunicode() {
        assert!(Pending::new(None).enabled());
        for v in ["0", "true", "01", " 1", ""] {
            assert!(!Pending::new(Some(v.as_ref())).enabled());
        }
        assert!(Pending::new(Some("1".as_ref())).enabled());
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert!(!Pending::new(Some(std::ffi::OsStr::from_bytes(&[255]))).enabled());
        }
    }
    #[test]
    fn burst_flags_or_and_one_ready_flush() {
        let mut p = Pending::new(Some("1".as_ref()));
        for _ in 0..8 {
            p.request(false, true);
        }
        p.request(true, false);
        assert_eq!(
            p.runnable(),
            Some(Request {
                hit_test: true,
                preserve_declutter: false
            })
        );
        p.finish(true);
        assert!(!p.dirty());
        assert_eq!(p.runnable(), None);
    }
    #[test]
    fn failed_flush_disarms_without_dropping_work() {
        let mut p = Pending::new(Some("1".as_ref()));
        p.request(true, false);
        p.finish(false);
        assert!(p.dirty());
        assert_eq!(p.runnable(), None);
        for _ in 0..8 {
            assert_eq!(p.runnable(), None);
        }
        p.explicit_retry();
        assert!(p.runnable().is_some());
        p.finish(true);
        assert!(!p.dirty());
    }
    #[test]
    fn newer_camera_event_retries_exact_accumulated_flags() {
        let mut p = Pending::new(Some("1".as_ref()));
        p.request(true, false);
        p.finish(false);
        p.request(false, true);
        assert_eq!(
            p.runnable(),
            Some(Request {
                hit_test: true,
                preserve_declutter: false
            })
        );
    }
    #[test]
    fn authoritative_clear_and_disabled_request() {
        let mut p = Pending::new(Some("1".as_ref()));
        p.request(false, true);
        p.finish(false);
        p.authoritative_ready();
        assert!(!p.dirty());
        assert_eq!(p.runnable(), None);
        let mut off = Pending::new(Some("0".as_ref()));
        off.request(true, false);
        assert!(!off.dirty());
    }
    #[test]
    fn failed_flush_plain_redraw_does_not_retry_but_real_consumer_and_motion_do() {
        let mut p = Pending::new(Some("1".as_ref()));
        p.request(true, false);
        p.finish(false);
        for _ in 0..500 {
            assert!(!p.plugin_ui_consumer(false, false));
            assert!(p.dirty());
            assert_eq!(p.runnable(), None);
        }
        assert!(p.plugin_ui_consumer(false, true));
        assert!(p.runnable().is_some());
        p.finish(false);
        assert!(p.plugin_ui_consumer(true, false));
        assert!(p.runnable().is_some());
        p.finish(false);
        assert!(!p.plugin_ui_consumer(false, false));
        p.request(false, true);
        assert_eq!(
            p.runnable(),
            Some(Request {
                hit_test: true,
                preserve_declutter: false
            })
        );
        p.finish(true);
        assert!(!p.dirty());
    }
}

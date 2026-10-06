//! Opt-in native test presentation. Hidden windows still provide native display
//! metrics and a Metal surface; snapshots use the renderer's texture readback.
use winit::{event_loop::EventLoopBuilder, window::WindowAttributes};

pub fn enabled() -> bool {
    std::env::var_os("FERRITE_BACKGROUND_TEST").is_some_and(|v| v == "1")
}

fn hidden_attributes(attributes: WindowAttributes) -> WindowAttributes {
    attributes
        .with_visible(false)
        .with_active(false)
        .with_maximized(false)
        .with_fullscreen(None)
}

pub fn window_attributes(attributes: WindowAttributes) -> WindowAttributes {
    if enabled() {
        hidden_attributes(attributes)
    } else {
        attributes
    }
}

pub fn configure_event_loop<T>(builder: &mut EventLoopBuilder<T>) {
    if !enabled() {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        // Prohibited also blocks accidental activation/visibility. No Dock or
        // menu entry is created, and the user's foreground app stays in charge.
        builder
            .with_activation_policy(ActivationPolicy::Prohibited)
            .with_activate_ignoring_other_apps(false)
            .with_default_menu(false);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = builder;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hidden_native_test_never_requests_visibility_focus_zoom_or_fullscreen() {
        let before = WindowAttributes::default()
            .with_visible(true)
            .with_active(true)
            .with_maximized(true)
            .with_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
        let after = hidden_attributes(before);
        assert!(!after.visible);
        assert!(!after.active);
        assert!(!after.maximized);
        assert!(after.fullscreen.is_none());
    }
}

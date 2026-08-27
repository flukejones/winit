//! `UIScene` lifecycle adoption, required by the iOS 27 SDK (Apple TN3187).
//!
//! Once scenes are adopted, UIKit replaces the app-level active/foreground/background
//! notifications with `UIScene*` equivalents, and a `UIWindow` only displays after it is attached
//! to a `UIWindowScene`. This module re-sources the lifecycle events from the scene notifications
//! and attaches winit's windows to the connected scene. Everything is notification observers — no
//! Objective-C scene-delegate class is needed; the application only declares a `UISceneDelegate`
//! class name in its `UIApplicationSceneManifest` to satisfy the launch requirement.

use objc2::rc::Retained;
use objc2::{msg_send, msg_send_id, ClassType};
use objc2_foundation::{
    MainThreadMarker, NSNotification, NSNotificationCenter, NSObject, NSString,
};
use objc2_ui_kit::{
    UIApplication, UISceneDidActivateNotification, UISceneDidDisconnectNotification,
    UISceneDidEnterBackgroundNotification, UISceneWillConnectNotification,
    UISceneWillDeactivateNotification, UISceneWillEnterForegroundNotification, UIWindow,
    UIWindowScene,
};

use super::app_state::{self, send_occluded_event_for_all_windows, EventWrapper};
use super::notification_center::create_observer;
use super::window::WinitUIWindow;
use crate::event::{Event, WindowEvent};
use crate::window::WindowId as RootWindowId;

/// The number of scene notifications observed (kept alive by the caller).
pub(crate) const SCENE_OBSERVER_COUNT: usize = 6;

/// The `UIWindowScene` the app's windows are attached to. Main-thread only (UIKit posts scene
/// notifications there); the `MainThreadMarker`-gated `static mut` follows `AppState`.
fn with_current_scene<R>(
    _mtm: MainThreadMarker,
    f: impl FnOnce(&mut Option<Retained<UIWindowScene>>) -> R,
) -> R {
    static mut CURRENT_WINDOW_SCENE: Option<Retained<UIWindowScene>> = None;
    #[allow(static_mut_refs)]
    unsafe {
        f(&mut CURRENT_WINDOW_SCENE)
    }
}

/// The notification's scene, but only if it is the application's own window scene. iOS 26's
/// keyboard connects an in-process `_UIKeyboardWindowScene` (a `UIWindowScene` subclass); adopting
/// it would reparent the app's windows into the keyboard's scene — black app window, no keyboard.
fn application_window_scene(notification: &NSNotification) -> Option<Retained<UIWindowScene>> {
    let scene = unsafe { notification.object() }?;
    let is_window_scene: bool =
        unsafe { msg_send![&*scene, isKindOfClass: UIWindowScene::class()] };
    if !is_window_scene {
        return None;
    }
    let session: Retained<NSObject> = unsafe { msg_send_id![&*scene, session] };
    let role: Retained<NSString> = unsafe { msg_send_id![&*session, role] };
    if role.to_string() != "UIWindowSceneSessionRoleApplication" {
        return None;
    }
    Some(unsafe { Retained::cast(scene) })
}

fn shared_application(mtm: MainThreadMarker) -> Retained<UIApplication> {
    let _ = mtm;
    unsafe { msg_send_id![UIApplication::class(), sharedApplication] }
}

/// Registry of the `WinitUIWindow`s winit has created. Windows are typically shown inside
/// `application:didFinishLaunching` — before `sceneWillConnect` — and a sceneless `UIWindow` is
/// absent from `UIApplication.windows()`, so it cannot be rediscovered when the scene connects.
/// Main-thread only; entries are retained for the process lifetime, like iOS app windows.
fn with_windows<R>(
    _mtm: MainThreadMarker,
    f: impl FnOnce(&mut Vec<Retained<WinitUIWindow>>) -> R,
) -> R {
    static mut WINDOWS: Vec<Retained<WinitUIWindow>> = Vec::new();
    #[allow(static_mut_refs)]
    unsafe {
        f(&mut WINDOWS)
    }
}

/// Record `window` and attach it to the scene if one has connected yet. Must run before the
/// caller's `makeKeyAndVisible`: a `UIWindow` without a window scene does not display under the
/// scene lifecycle.
pub(crate) fn attach_to_scene(mtm: MainThreadMarker, window: &Retained<WinitUIWindow>) {
    with_windows(mtm, |windows| {
        if !windows.iter().any(|w| Retained::as_ptr(w) == Retained::as_ptr(window)) {
            windows.push(window.clone());
        }
    });
    with_current_scene(mtm, |scene| {
        if let Some(scene) = scene {
            fit_window_to_scene(window, scene);
        }
    });
}

/// Attach `window` to `scene` and size it to the scene's screen. `Window::new` builds the frame
/// from the deprecated `UIScreen::mainScreen` bounds, which can be 0×0 early in launch, and a
/// `UIWindow` does not auto-resize to its window scene — a zero frame leaves the render surface
/// black. Sizing from the scene's screen triggers layout, reported to the app as `Resized`.
fn fit_window_to_scene(window: &UIWindow, scene: &UIWindowScene) {
    unsafe { window.setWindowScene(Some(scene)) };
    let bounds = unsafe { scene.screen() }.bounds();
    window.setFrame(bounds);
}

/// Attach every registered window to the scene and (re-)present it — windows shown while
/// sceneless are not actually on screen yet.
fn attach_registered_windows(mtm: MainThreadMarker, scene: &UIWindowScene) {
    with_windows(mtm, |windows| {
        for window in windows.iter() {
            fit_window_to_scene(window, scene);
            window.makeKeyAndVisible();
        }
    });
}

/// Report focus from scene activation.
fn send_focused_to_all(mtm: MainThreadMarker, focused: bool) {
    with_windows(mtm, |windows| {
        for window in windows.iter() {
            app_state::handle_nonuser_event(
                mtm,
                EventWrapper::StaticEvent(Event::WindowEvent {
                    window_id: RootWindowId(window.id()),
                    event: WindowEvent::Focused(focused),
                }),
            );
        }
    });
}

/// Register observers for the `UIScene` lifecycle notifications. The returned
/// observers must be kept alive for as long as events should be delivered.
pub(crate) fn create_scene_observers(
    center: &NSNotificationCenter,
    mtm: MainThreadMarker,
) -> [Retained<NSObject>; SCENE_OBSERVER_COUNT] {
    let will_connect =
        create_observer(center, unsafe { UISceneWillConnectNotification }, move |n| {
            let Some(window_scene) = application_window_scene(n) else { return };
            with_current_scene(mtm, |slot| *slot = Some(window_scene.clone()));
            attach_registered_windows(mtm, &window_scene);
        });

    let did_activate =
        create_observer(center, unsafe { UISceneDidActivateNotification }, move |n| {
            if application_window_scene(n).is_none() {
                return;
            }
            app_state::handle_nonuser_event(mtm, EventWrapper::StaticEvent(Event::Resumed));
            send_focused_to_all(mtm, true);
        });

    let will_deactivate =
        create_observer(center, unsafe { UISceneWillDeactivateNotification }, move |n| {
            if application_window_scene(n).is_none() {
                return;
            }
            send_focused_to_all(mtm, false);
            app_state::handle_nonuser_event(mtm, EventWrapper::StaticEvent(Event::Suspended));
        });

    let will_foreground =
        create_observer(center, unsafe { UISceneWillEnterForegroundNotification }, move |n| {
            if application_window_scene(n).is_none() {
                return;
            }
            send_occluded_event_for_all_windows(&shared_application(mtm), false);
        });

    let did_background =
        create_observer(center, unsafe { UISceneDidEnterBackgroundNotification }, move |n| {
            if application_window_scene(n).is_none() {
                return;
            }
            send_occluded_event_for_all_windows(&shared_application(mtm), true);
        });

    let did_disconnect =
        create_observer(center, unsafe { UISceneDidDisconnectNotification }, move |n| {
            if application_window_scene(n).is_none() {
                return;
            }
            with_current_scene(mtm, |slot| *slot = None);
        });

    [will_connect, did_activate, will_deactivate, will_foreground, did_background, did_disconnect]
}

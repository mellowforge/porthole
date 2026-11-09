//! macOS: Dock ▸ Quit, logging out and restarting ask the app delegate
//! `applicationShouldTerminate:` before ending the app. tao's delegate doesn't implement it,
//! so AppKit used to terminate at once and skip the quit sequence (unsaved grid edits lost,
//! open transactions rolled back by the server, without asking).
//!
//! `install` adds that method to tao's delegate class at launch. When no window would ask
//! anything (no unsaved edits, open transaction, running save or export, or unwritten tab state),
//! it answers "terminate now", so Dock ▸ Quit and logout behave as before. Otherwise it unhides
//! and activates the app, cancels the system's terminate and starts the same quit sequence ⌘Q
//! uses (`windows::start_quit`), which asks each window and ends with `app.exit(0)` when every
//! window agrees. tao exits by stopping its run loop, not through `terminate:`, so the hook isn't
//! asked again.
//!
//! In that case a logout or restart is interrupted (macOS says the app stopped it); the user
//! answers the app's prompt and logs out again. (Answering "later" and replying once the
//! sequence ends would avoid that; see the follow-ups note.)

use std::ffi::CStr;
use std::sync::OnceLock;

use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
use objc2::{class, ffi, msg_send, sel};

/// `NSApplicationTerminateReply.NSTerminateCancel`.
const TERMINATE_CANCEL: usize = 0;
/// `NSApplicationTerminateReply.NSTerminateNow`.
const TERMINATE_NOW: usize = 1;
/// The method's Objective-C type encoding: returns NSUInteger, takes self, _cmd and an id.
const SHOULD_TERMINATE_TYPES: &CStr = c"Q@:@";

/// The answer to a system terminate request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    /// Nothing to ask: let the system end the app now.
    Now,
    /// The app runs its own quit sequence instead, and quits itself when every window agrees.
    Cancel,
}

/// Decides what a system terminate request does.
static ON_TERMINATE: OnceLock<Box<dyn Fn() -> Reply + Send + Sync>> = OnceLock::new();

extern "C-unwind" fn should_terminate(_this: *mut AnyObject, _cmd: Sel, _sender: *mut AnyObject) -> usize {
    let Some(decide) = ON_TERMINATE.get() else {
        // Not installed: behave as AppKit would without the method.
        return TERMINATE_NOW;
    };
    // A panic must not unwind into AppKit; asking (Cancel) is the safe answer.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(decide)) {
        Ok(Reply::Now) => TERMINATE_NOW,
        Ok(Reply::Cancel) => TERMINATE_CANCEL,
        Err(_) => {
            eprintln!("deciding a system quit panicked; the quit is canceled");
            TERMINATE_CANCEL
        }
    }
}

/// Unhides the app and brings it to the front, so a quit prompt in a window can be seen
/// (Dock ▸ Quit works while the app is hidden with ⌘H). Main thread only.
pub fn bring_app_to_front() {
    // SAFETY: plain NSApplication calls on the main thread (the terminate hook runs there).
    unsafe {
        let ns_app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![ns_app, unhide: std::ptr::null_mut::<AnyObject>()];
        let _: () = msg_send![ns_app, activateIgnoringOtherApps: Bool::YES];
    }
}

/// Adds `applicationShouldTerminate:` to `cls`. False when the class already has it (or
/// the runtime refused), in which case the existing method is left alone.
///
/// # Safety
/// `cls` must be a valid, registered Objective-C class.
unsafe fn add_should_terminate(cls: *mut AnyClass) -> bool {
    // SAFETY: `should_terminate`'s signature matches SHOULD_TERMINATE_TYPES; the runtime
    // calls it through the type-erased `Imp` with exactly those arguments.
    let imp: Imp = unsafe {
        std::mem::transmute::<extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> usize, Imp>(
            should_terminate,
        )
    };
    unsafe { ffi::class_addMethod(cls, sel!(applicationShouldTerminate:), imp, SHOULD_TERMINATE_TYPES.as_ptr()) }
        .as_bool()
}

/// Routes system terminate requests to `decide`. Call once, on the main thread, after the app
/// finished launching (tao's delegate exists then).
pub fn install(decide: impl Fn() -> Reply + Send + Sync + 'static) {
    if ON_TERMINATE.set(Box::new(decide)).is_err() {
        return;
    }
    // SAFETY: called on the main thread after launch; `sharedApplication` and `delegate`
    // are plain getters, and the delegate's class is a registered class.
    unsafe {
        let ns_app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let delegate: *mut AnyObject = msg_send![ns_app, delegate];
        if delegate.is_null() {
            eprintln!("Dock ▸ Quit can't ask about unsaved changes: the app has no delegate");
            return;
        }
        let cls = ffi::object_getClass(delegate) as *mut AnyClass;
        if !add_should_terminate(cls) {
            eprintln!("Dock ▸ Quit can't ask about unsaved changes: the app delegate already answers it");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use objc2::runtime::{AnyObject, ClassBuilder, NSObject};
    use objc2::{msg_send, ClassType};

    use super::*;

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn a_system_terminate_is_canceled_and_starts_the_quit_sequence() {
        // A stand-in for tao's delegate class, without the method.
        let builder = ClassBuilder::new(c"PortholeTestDelegate", NSObject::class()).unwrap();
        let cls = builder.register() as *const AnyClass as *mut AnyClass;

        // Before install, the hook behaves like AppKit without the method.
        assert_eq!(should_terminate(std::ptr::null_mut(), sel!(applicationShouldTerminate:), std::ptr::null_mut()), TERMINATE_NOW);

        // The first request has something to ask (Cancel), the second doesn't (Now).
        assert!(ON_TERMINATE.set(Box::new(|| {
            if CALLS.fetch_add(1, Ordering::SeqCst) == 0 { Reply::Cancel } else { Reply::Now }
        }))
        .is_ok());
        assert!(unsafe { add_should_terminate(cls) });
        // A second add is refused and leaves the first in place.
        assert!(!unsafe { add_should_terminate(cls) });

        // Sent through the Objective-C runtime, as AppKit sends it.
        let obj: *mut AnyObject = unsafe { msg_send![&*cls, new] };
        let reply: usize = unsafe { msg_send![obj, applicationShouldTerminate: std::ptr::null_mut::<AnyObject>()] };
        assert_eq!(reply, TERMINATE_CANCEL);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        let reply: usize = unsafe { msg_send![obj, applicationShouldTerminate: std::ptr::null_mut::<AnyObject>()] };
        assert_eq!(reply, TERMINATE_NOW);
        let _: () = unsafe { msg_send![obj, release] };
    }
}

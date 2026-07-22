use crate::application_core::{ApplicationCore, domain::LifecycleStopReason};

#[cfg(target_os = "macos")]
mod macos {
    use std::{cell::RefCell, ptr::NonNull};

    use block2::RcBlock;
    use objc2::{
        rc::Retained,
        runtime::{AnyObject, ProtocolObject},
    };
    use objc2_app_kit::{NSWorkspace, NSWorkspaceWillSleepNotification};
    use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol};

    use super::{ApplicationCore, LifecycleStopReason};

    thread_local! {
        static SLEEP_OBSERVER: RefCell<Option<WorkspaceSleepObserver>> = const { RefCell::new(None) };
    }

    struct WorkspaceSleepObserver {
        center: Retained<NSNotificationCenter>,
        token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
    }

    impl Drop for WorkspaceSleepObserver {
        fn drop(&mut self) {
            let token =
                <ProtocolObject<dyn NSObjectProtocol> as AsRef<AnyObject>>::as_ref(&self.token);
            // SAFETY: The token was returned by this notification center and remains retained.
            unsafe { self.center.removeObserver(token) };
        }
    }

    pub fn install(core: ApplicationCore) {
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        let block = RcBlock::new(move |_notification: NonNull<NSNotification>| {
            let core = core.clone();
            tauri::async_runtime::spawn(async move {
                let _ = core.shutdown(LifecycleStopReason::SystemSleep).await;
            });
        });
        // SAFETY: AppKit supplies this notification and callback argument.
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSWorkspaceWillSleepNotification),
                None,
                None,
                &block,
            )
        };
        SLEEP_OBSERVER.with(|slot| {
            *slot.borrow_mut() = Some(WorkspaceSleepObserver { center, token });
        });
    }
}

#[cfg(target_os = "macos")]
pub use macos::install;

#[cfg(not(target_os = "macos"))]
pub fn install(_core: ApplicationCore) {}

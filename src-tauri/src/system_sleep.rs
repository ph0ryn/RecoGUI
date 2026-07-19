use crate::supervisor::EngineSupervisor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspacePowerNotification {
    WillSleep,
    DidWake,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PowerAction {
    StopActiveSession,
    Ignore,
}

fn action_for_notification(notification: WorkspacePowerNotification) -> PowerAction {
    match notification {
        WorkspacePowerNotification::WillSleep => PowerAction::StopActiveSession,
        WorkspacePowerNotification::DidWake => PowerAction::Ignore,
    }
}

fn active_session_from_snapshot(snapshot: &serde_json::Value) -> Option<&str> {
    snapshot
        .as_object()?
        .get("activeSession")?
        .as_str()
        .filter(|session_id| !session_id.is_empty())
}

async fn handle_power_notification(
    notification: WorkspacePowerNotification,
    supervisor: EngineSupervisor,
) {
    if action_for_notification(notification) != PowerAction::StopActiveSession
        || !supervisor.is_running().await
    {
        return;
    }
    let Ok(snapshot) = supervisor
        .request("engine.getState", None, serde_json::json!({}))
        .await
    else {
        return;
    };
    let Some(session_id) = active_session_from_snapshot(&snapshot).map(ToOwned::to_owned) else {
        return;
    };
    let _ = supervisor
        .request(
            "session.stop",
            Some(session_id.clone()),
            serde_json::json!({
                "sessionId": session_id,
                "reason": "systemSleep",
                "context": { "source": "macOSWorkspaceNotification" }
            }),
        )
        .await;
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{cell::RefCell, ptr::NonNull};

    use block2::RcBlock;
    use objc2::{
        rc::Retained,
        runtime::{AnyObject, ProtocolObject},
    };
    use objc2_app_kit::{
        NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceWillSleepNotification,
    };
    use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol};

    use super::{EngineSupervisor, WorkspacePowerNotification, handle_power_notification};

    thread_local! {
        static SLEEP_OBSERVER: RefCell<Option<WorkspaceSleepObserver>> = const { RefCell::new(None) };
    }

    struct WorkspaceSleepObserver {
        center: Retained<NSNotificationCenter>,
        tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    }

    impl Drop for WorkspaceSleepObserver {
        fn drop(&mut self) {
            // SAFETY: Each token was returned by this notification center's
            // block registration API and remains retained by this owner.
            for token in &self.tokens {
                let token =
                    <ProtocolObject<dyn NSObjectProtocol> as AsRef<AnyObject>>::as_ref(token);
                unsafe { self.center.removeObserver(token) };
            }
        }
    }

    pub fn install(supervisor: EngineSupervisor) {
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        let sleep_supervisor = supervisor.clone();
        let block = RcBlock::new(move |_notification: NonNull<NSNotification>| {
            let supervisor = sleep_supervisor.clone();
            tauri::async_runtime::spawn(async move {
                handle_power_notification(WorkspacePowerNotification::WillSleep, supervisor).await;
            });
        });
        // SAFETY: The notification name and callback argument are provided by
        // AppKit. The copied block is Send and owns only a thread-safe host handle.
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSWorkspaceWillSleepNotification),
                None,
                None,
                &block,
            )
        };
        let wake_block = RcBlock::new(move |_notification: NonNull<NSNotification>| {
            let supervisor = supervisor.clone();
            tauri::async_runtime::spawn(async move {
                handle_power_notification(WorkspacePowerNotification::DidWake, supervisor).await;
            });
        });
        // Wake is observed only to enforce the explicit no-auto-resume path.
        let wake_token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSWorkspaceDidWakeNotification),
                None,
                None,
                &wake_block,
            )
        };
        SLEEP_OBSERVER.with(|slot| {
            *slot.borrow_mut() = Some(WorkspaceSleepObserver {
                center,
                tokens: vec![token, wake_token],
            });
        });
    }
}

#[cfg(target_os = "macos")]
pub use macos::install;

#[cfg(not(target_os = "macos"))]
pub fn install(_supervisor: EngineSupervisor) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_stops_the_active_session() {
        assert_eq!(
            action_for_notification(WorkspacePowerNotification::WillSleep),
            PowerAction::StopActiveSession
        );
    }

    #[test]
    fn wake_never_auto_resumes() {
        assert_eq!(
            action_for_notification(WorkspacePowerNotification::DidWake),
            PowerAction::Ignore
        );
    }

    #[test]
    fn idle_snapshot_has_no_session_to_stop() {
        let snapshot = serde_json::json!({
            "engineState": "idle",
            "activeSession": null
        });

        assert_eq!(active_session_from_snapshot(&snapshot), None);
    }

    #[test]
    fn active_snapshot_yields_the_session_to_stop() {
        let snapshot = serde_json::json!({
            "engineState": "running",
            "activeSession": "5fcd9ed3-0d52-466f-adbc-b9da5b75b824"
        });

        assert_eq!(
            active_session_from_snapshot(&snapshot),
            Some("5fcd9ed3-0d52-466f-adbc-b9da5b75b824")
        );
    }
}

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::Mutex;

use crate::supervisor::EngineSupervisor;

const CLOSE_REQUESTED_EVENT: &str = "host://close-requested";
const CLOSE_FORCE_REQUIRED_EVENT: &str = "host://close-force-required";

#[derive(Debug, Clone, PartialEq, Eq)]
enum CloseState {
    Idle,
    Checking,
    AwaitingDecision { session_id: String },
    Stopping { session_id: String },
    AwaitingForce { session_id: Option<String> },
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CloseResolution {
    Cancel,
    StopAndQuit,
    ForceQuit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseTransition {
    Cancelled,
    StopAndQuit,
    ForceQuit,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveCloseRequest {
    pub resolution: CloseResolution,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CloseRequestedEvent {
    session_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CloseForceRequiredEvent {
    session_id: Option<String>,
    error: String,
}

#[derive(Debug, Clone)]
pub struct CloseCoordinator {
    inner: Arc<CloseCoordinatorInner>,
}

#[derive(Debug)]
struct CloseCoordinatorInner {
    state: Mutex<CloseState>,
    allow_native_close: AtomicBool,
}

impl Default for CloseCoordinator {
    fn default() -> Self {
        Self {
            inner: Arc::new(CloseCoordinatorInner {
                state: Mutex::new(CloseState::Idle),
                allow_native_close: AtomicBool::new(false),
            }),
        }
    }
}

impl CloseCoordinator {
    pub fn allows_native_close(&self) -> bool {
        self.inner.allow_native_close.load(Ordering::Acquire)
    }

    pub async fn begin(&self, app: AppHandle, supervisor: EngineSupervisor) {
        {
            let mut state = self.inner.state.lock().await;
            if *state != CloseState::Idle {
                return;
            }
            *state = CloseState::Checking;
        }

        if !supervisor.is_running().await {
            self.quit_normally(app, supervisor).await;
            return;
        }
        let snapshot = match supervisor
            .request("engine.getState", None, serde_json::json!({}))
            .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                *self.inner.state.lock().await = CloseState::AwaitingForce { session_id: None };
                let _ = app.emit(
                    CLOSE_FORCE_REQUIRED_EVENT,
                    CloseForceRequiredEvent {
                        session_id: None,
                        error: error.to_string(),
                    },
                );
                return;
            }
        };
        let active_session = active_session_from_snapshot(&snapshot).map(ToOwned::to_owned);

        let Some(session_id) = active_session else {
            self.quit_normally(app, supervisor).await;
            return;
        };
        *self.inner.state.lock().await = CloseState::AwaitingDecision {
            session_id: session_id.clone(),
        };
        let _ = app.emit(CLOSE_REQUESTED_EVENT, CloseRequestedEvent { session_id });
    }

    async fn resolve(
        &self,
        resolution: CloseResolution,
    ) -> Result<(CloseTransition, Option<String>), String> {
        let mut state = self.inner.state.lock().await;
        let (transition, next) = transition_for_resolution(&state, resolution)?;
        let session_id = match &*state {
            CloseState::AwaitingDecision { session_id } => Some(session_id.clone()),
            CloseState::AwaitingForce { session_id } => session_id.clone(),
            _ => None,
        };
        *state = next;
        Ok((transition, session_id))
    }

    async fn stop_and_quit(
        &self,
        app: AppHandle,
        supervisor: EngineSupervisor,
        session_id: String,
    ) {
        let result = supervisor
            .request(
                "session.stop",
                Some(session_id.clone()),
                serde_json::json!({
                    "sessionId": session_id,
                    "reason": "appQuit",
                    "context": { "source": "windowCloseRequest" }
                }),
            )
            .await;
        match result {
            Ok(_) => self.quit_normally(app, supervisor).await,
            Err(error) => {
                let session_id = match &*self.inner.state.lock().await {
                    CloseState::Stopping { session_id } => session_id.clone(),
                    _ => return,
                };
                *self.inner.state.lock().await = CloseState::AwaitingForce {
                    session_id: Some(session_id.clone()),
                };
                let _ = app.emit(
                    CLOSE_FORCE_REQUIRED_EVENT,
                    CloseForceRequiredEvent {
                        session_id: Some(session_id),
                        error: error.to_string(),
                    },
                );
            }
        }
    }

    async fn quit_normally(&self, app: AppHandle, supervisor: EngineSupervisor) {
        supervisor.shutdown().await;
        self.inner.allow_native_close.store(true, Ordering::Release);
        app.exit(0);
    }

    async fn force_quit(&self, app: AppHandle, supervisor: EngineSupervisor) {
        supervisor.force_terminate().await;
        self.inner.allow_native_close.store(true, Ordering::Release);
        app.exit(1);
    }
}

fn active_session_from_snapshot(snapshot: &Value) -> Option<&str> {
    snapshot
        .as_object()?
        .get("activeSession")?
        .as_str()
        .filter(|session_id| !session_id.is_empty())
}

fn transition_for_resolution(
    state: &CloseState,
    resolution: CloseResolution,
) -> Result<(CloseTransition, CloseState), String> {
    match (state, resolution) {
        (CloseState::AwaitingDecision { .. }, CloseResolution::Cancel) => {
            Ok((CloseTransition::Cancelled, CloseState::Idle))
        }
        (CloseState::AwaitingDecision { session_id }, CloseResolution::StopAndQuit) => Ok((
            CloseTransition::StopAndQuit,
            CloseState::Stopping {
                session_id: session_id.clone(),
            },
        )),
        (CloseState::AwaitingForce { session_id }, CloseResolution::ForceQuit) => Ok((
            CloseTransition::ForceQuit,
            CloseState::AwaitingForce {
                session_id: session_id.clone(),
            },
        )),
        _ => Err("close resolution is not valid in the current state".into()),
    }
}

#[tauri::command]
pub async fn host_resolve_close_request(
    input: ResolveCloseRequest,
    app: AppHandle,
    coordinator: State<'_, CloseCoordinator>,
    supervisor: State<'_, EngineSupervisor>,
) -> Result<(), String> {
    let (transition, session_id) = coordinator.resolve(input.resolution).await?;
    match transition {
        CloseTransition::Cancelled => Ok(()),
        CloseTransition::StopAndQuit => {
            let session_id =
                session_id.ok_or_else(|| "active session is unavailable".to_owned())?;
            let coordinator = coordinator.inner().clone();
            let supervisor = supervisor.inner().clone();
            tauri::async_runtime::spawn(async move {
                coordinator.stop_and_quit(app, supervisor, session_id).await;
            });
            Ok(())
        }
        CloseTransition::ForceQuit => {
            let coordinator = coordinator.inner().clone();
            let supervisor = supervisor.inner().clone();
            tauri::async_runtime::spawn(async move {
                coordinator.force_quit(app, supervisor).await;
            });
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_state() -> CloseState {
        CloseState::AwaitingDecision {
            session_id: "session-1".into(),
        }
    }

    #[test]
    fn idle_snapshot_closes_without_a_decision() {
        assert_eq!(
            active_session_from_snapshot(&serde_json::json!({ "activeSession": null })),
            None
        );
    }

    #[test]
    fn active_snapshot_requires_a_decision() {
        assert_eq!(
            active_session_from_snapshot(&serde_json::json!({ "activeSession": "session-1" })),
            Some("session-1")
        );
    }

    #[test]
    fn cancel_returns_to_idle() {
        assert_eq!(
            transition_for_resolution(&active_state(), CloseResolution::Cancel).unwrap(),
            (CloseTransition::Cancelled, CloseState::Idle)
        );
    }

    #[test]
    fn confirm_enters_stopping_state() {
        assert_eq!(
            transition_for_resolution(&active_state(), CloseResolution::StopAndQuit).unwrap(),
            (
                CloseTransition::StopAndQuit,
                CloseState::Stopping {
                    session_id: "session-1".into()
                }
            )
        );
    }

    #[test]
    fn force_quit_requires_the_secondary_state() {
        assert!(transition_for_resolution(&active_state(), CloseResolution::ForceQuit).is_err());
        assert!(
            transition_for_resolution(
                &CloseState::AwaitingForce {
                    session_id: Some("session-1".into())
                },
                CloseResolution::ForceQuit
            )
            .is_ok()
        );
    }
}

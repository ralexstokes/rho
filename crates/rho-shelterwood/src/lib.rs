//! Shelterwood supervision for dynamically attached rho sessions.
//!
//! The fixed root owns a non-restartable dynamic `sessions` scope. Each
//! attached session gets its own ordered subtree with a restartable `control`
//! actor. The outer subtree is the session's fate-sharing and removal unit;
//! the control actor can restart in place while preserving its mailbox
//! membership and cloned durable-store arguments.

use std::time::Duration;

pub use shelterwood::{
    Actor, ActorDef, ActorRef, CallError, CallErrorKind,
    CancellationToken as ShelterwoodCancellationToken, ChildId, Context, ExitError, ExitResult,
    Incarnation, Mailbox, RemoveOutcome, Reply, RestartPolicy, StopContext,
};
use shelterwood::{
    BuildError, ChildState, DynamicScopeRef, DynamicTree, ReserveError, ScopeRef, ShutdownTimeout,
    StartOrShutdownError, SubtreeOnceDef, System, Tree, WaitError,
};
use thiserror::Error;
use tokio::sync::Mutex;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Failure to build, mutate, observe, or stop the supervised session tree.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SupervisorError {
    /// A fixed tree declaration was invalid.
    #[error(transparent)]
    Declaration(#[from] ReserveError),
    /// The tree could not be lowered into the ambient runtime.
    #[error(transparent)]
    Build(#[from] BuildError),
    /// Initial root startup failed and was rolled back.
    #[error(transparent)]
    Start(#[from] StartOrShutdownError),
    /// A runtime session subtree could not be admitted.
    #[error("session subtree admission failed: {0}")]
    Admission(ReserveError),
    /// Observation ended or timed out before the actor reached a decisive state.
    #[error(transparent)]
    Wait(#[from] WaitError),
    /// A session actor terminalized before becoming ready.
    #[error("session actor failed during startup: {0}")]
    ActorStartup(String),
    /// The root exceeded its cooperative shutdown grace.
    #[error(transparent)]
    Shutdown(#[from] ShutdownTimeout),
}

/// Exact membership handles for one session subtree and its control actor.
pub struct SessionHandle<M> {
    scope: ScopeRef,
    actor: ActorRef<M>,
}

impl<M> Clone for SessionHandle<M> {
    fn clone(&self) -> Self {
        Self {
            scope: self.scope.clone(),
            actor: self.actor.clone(),
        }
    }
}

impl<M> SessionHandle<M> {
    /// Returns the membership-addressed control actor.
    #[must_use]
    pub fn actor(&self) -> &ActorRef<M> {
        &self.actor
    }

    /// Returns the session's fate-sharing scope.
    #[must_use]
    pub fn scope(&self) -> &ScopeRef {
        &self.scope
    }
}

/// A running fixed root with a dynamic scope for session subtrees.
pub struct SupervisedSessions {
    system: Mutex<Option<System<ScopeRef>>>,
    sessions: DynamicScopeRef,
}

impl SupervisedSessions {
    /// Starts an empty supervised root and waits for aggregate readiness.
    pub async fn start() -> Result<Self, SupervisorError> {
        let mut root = Tree::new();
        let sessions =
            root.add_subtree_once("sessions", SubtreeOnceDef::new(DynamicTree::new()))?;
        let system = root.spawn()?.start_or_shutdown(SHUTDOWN_GRACE).await?;
        Ok(Self {
            system: Mutex::new(Some(system)),
            sessions,
        })
    }

    /// Adds one restartable session control actor inside its own subtree.
    ///
    /// Admission is not readiness. This waits until `control` is either
    /// running or terminal, so callers never mistake a reserved membership
    /// for a usable session.
    pub async fn add<A>(
        &self,
        id: impl Into<ChildId>,
        definition: ActorDef<A>,
    ) -> Result<SessionHandle<A::Msg>, SupervisorError>
    where
        A: Actor,
    {
        let mut tree = Tree::new();
        let actor = tree.add_actor("control", definition)?;
        let scope = self
            .sessions
            .add_subtree_once(id, SubtreeOnceDef::new(tree))
            .await
            .map_err(SupervisorError::Admission)?;
        let child = scope
            .wait_for_child(
                "control",
                |child| matches!(child.state, ChildState::Running) || child.state.is_terminal(),
                Duration::MAX,
            )
            .await?;
        if !matches!(child.state, ChildState::Running) {
            let state = format!("{:?}", child.state);
            let _ = self.sessions.remove_scope(&scope).await;
            return Err(SupervisorError::ActorStartup(state));
        }
        Ok(SessionHandle { scope, actor })
    }

    /// Removes exactly this session membership, never a same-name successor.
    pub async fn remove<M>(&self, session: &SessionHandle<M>) -> RemoveOutcome {
        self.sessions.remove_scope(&session.scope).await
    }

    /// Waits for the same control membership to publish a newer incarnation.
    pub async fn wait_for_replacement<M>(
        &self,
        session: &SessionHandle<M>,
        previous: Incarnation,
        timeout: Duration,
    ) -> Result<Incarnation, SupervisorError> {
        let child = session
            .scope
            .wait_for_child(
                "control",
                move |child| {
                    child.state.is_terminal()
                        || child
                            .incarnation
                            .is_some_and(|current| current.supersedes(previous))
                },
                timeout,
            )
            .await?;
        if child.state.is_terminal() {
            return Err(SupervisorError::ActorStartup(format!("{:?}", child.state)));
        }
        child.incarnation.ok_or_else(|| {
            SupervisorError::ActorStartup("replacement has no incarnation".to_owned())
        })
    }

    /// Cooperatively stops and joins the entire tree once.
    pub async fn shutdown(&self) -> Result<(), SupervisorError> {
        let system = self.system.lock().await.take();
        if let Some(system) = system {
            system.shutdown(SHUTDOWN_GRACE).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    enum Message {
        Crash,
        Generation(Reply<usize>),
    }

    struct RestartingActor {
        generation: usize,
    }

    impl Actor for RestartingActor {
        type Msg = Message;
        type Args = Arc<AtomicUsize>;

        async fn init(
            generations: Self::Args,
            _: &mut Context<'_, Self>,
        ) -> Result<Self, ExitError> {
            Ok(Self {
                generation: generations.fetch_add(1, Ordering::SeqCst) + 1,
            })
        }

        async fn handle(&mut self, message: Self::Msg, _: &mut Context<'_, Self>) -> ExitResult {
            match message {
                Message::Crash => panic!("injected actor crash"),
                Message::Generation(reply) => reply.send(self.generation),
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_session_actor_restarts_in_place_with_fenced_calls() {
        let sessions = SupervisedSessions::start().await.unwrap();
        let generations = Arc::new(AtomicUsize::new(0));
        let session = sessions
            .add(
                "session-test",
                ActorDef::<RestartingActor>::cloned(Arc::clone(&generations))
                    .mailbox(Mailbox::queue(16).unwrap())
                    .restart(RestartPolicy::default()),
            )
            .await
            .unwrap();
        let first = session
            .actor()
            .call(Message::Generation, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(first.value, 1);

        let accepted = session
            .actor()
            .send(Message::Crash)
            .await
            .expect("crash request is accepted by the first incarnation");
        assert_eq!(accepted, first.incarnation);
        let replacement = sessions
            .wait_for_replacement(&session, accepted, Duration::from_secs(2))
            .await
            .unwrap();
        assert!(replacement.supersedes(accepted));
        let second = session
            .actor()
            .call(Message::Generation, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(second.value, 2);
        assert_eq!(second.incarnation, replacement);

        assert_eq!(sessions.remove(&session).await, RemoveOutcome::Removed);
        sessions.shutdown().await.unwrap();
    }
}

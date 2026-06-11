use tokio_util::sync::CancellationToken;

tokio::task_local! {
    /// Ambient cancellation token for the tool call currently executing on
    /// this task. Set by the core turn loop around each `call_tool` future;
    /// rig 0.37 awaits `Tool::call` inline in the caller's future (no spawn),
    /// so the task-local is visible inside tool implementations.
    static TOOL_CANCEL_TOKEN: CancellationToken;
}

/// Run `fut` with `token` visible as the ambient tool cancellation token for
/// every tool called inside it.
pub async fn with_tool_cancel_scope<F>(token: CancellationToken, fut: F) -> F::Output
where
    F: Future,
{
    TOOL_CANCEL_TOKEN.scope(token, fut).await
}

/// The ambient cancellation token for the current tool call. Outside a scope
/// (tests, stub drivers, or if the executor ever moves tool futures to a
/// separate task) this degrades gracefully to a token that never cancels.
pub fn tool_cancel_token() -> CancellationToken {
    TOOL_CANCEL_TOKEN
        .try_with(CancellationToken::clone)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scope_exposes_the_given_token_to_nested_calls() {
        let token = CancellationToken::new();
        token.cancel();
        let observed_cancelled =
            with_tool_cancel_scope(token, async { tool_cancel_token().is_cancelled() }).await;
        assert!(observed_cancelled);
    }

    #[tokio::test]
    async fn outside_scope_returns_a_never_cancelled_token() {
        assert!(!tool_cancel_token().is_cancelled());
    }

    #[tokio::test]
    async fn cancelling_after_scope_entry_is_observed_inside() {
        let token = CancellationToken::new();
        let inner = token.clone();
        let handle = tokio::spawn(with_tool_cancel_scope(inner, async {
            tool_cancel_token().cancelled().await;
            true
        }));
        token.cancel();
        assert!(handle.await.unwrap_or(false));
    }
}

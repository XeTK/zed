//! Which threads another program is currently driving.
//!
//! A program that claims a thread is the one sending it messages. While the
//! claim lasts, Zed locks the thread's own message box, so the two cannot both
//! write to the same session. A claim ends when it is released, when you take
//! the thread back, or when it times out.

use crate::thread_metadata_store::ThreadId;
use anyhow::{Result, bail};
use gpui::{App, BorrowAppContext as _, Global};
use std::{collections::HashMap, time::Duration};

pub const DEFAULT_CLAIM: Duration = Duration::from_secs(10 * 60);
pub const MAX_CLAIM: Duration = Duration::from_secs(60 * 60);

struct Claim {
    client: String,
    /// Renewing a claim starts a new one, so an older timer cannot end it.
    generation: u64,
}

#[derive(Default)]
pub struct ThreadClaims {
    claims: HashMap<ThreadId, Claim>,
    next_generation: u64,
}

impl Global for ThreadClaims {}

pub fn init(cx: &mut App) {
    cx.set_global(ThreadClaims::default());
}

/// Who is driving the thread right now, if anyone.
pub fn holder(thread_id: ThreadId, cx: &App) -> Option<String> {
    let claims = cx.try_global::<ThreadClaims>()?;
    Some(claims.claims.get(&thread_id)?.client.clone())
}

/// Claims `thread_id` for `client`, or renews that client's claim. Fails if
/// another client holds it.
pub fn claim(thread_id: ThreadId, client: &str, duration: Duration, cx: &mut App) -> Result<()> {
    if let Some(holder) = holder(thread_id, cx)
        && holder != client
    {
        bail!("the thread is already being driven by {holder}");
    }
    let duration = duration.min(MAX_CLAIM);
    let generation = cx.update_global::<ThreadClaims, _>(|claims, _| {
        claims.next_generation += 1;
        let generation = claims.next_generation;
        claims.claims.insert(
            thread_id,
            Claim {
                client: client.to_string(),
                generation,
            },
        );
        generation
    });

    // Ends the claim when its time is up, even if nobody asks, so the thread's
    // view unlocks by itself.
    cx.spawn(async move |cx| {
        cx.background_executor().timer(duration).await;
        cx.update(|cx| expire(thread_id, generation, cx));
    })
    .detach();
    Ok(())
}

/// Ends the claim. With a client, only if that client holds it; with `None`
/// (you taking the thread back), whoever holds it. Returns whether one ended.
pub fn release(thread_id: ThreadId, client: Option<&str>, cx: &mut App) -> bool {
    let held_by_someone_else =
        client.is_some_and(|client| holder(thread_id, cx).is_some_and(|holder| holder != client));
    if held_by_someone_else {
        return false;
    }
    cx.update_global::<ThreadClaims, _>(|claims, _| claims.claims.remove(&thread_id).is_some())
}

fn expire(thread_id: ThreadId, generation: u64, cx: &mut App) {
    cx.update_global::<ThreadClaims, _>(|claims, _| {
        if claims
            .claims
            .get(&thread_id)
            .is_some_and(|claim| claim.generation == generation)
        {
            claims.claims.remove(&thread_id);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(init);
    }

    #[gpui::test]
    fn test_a_claim_blocks_other_clients_until_released(cx: &mut TestAppContext) {
        init_test(cx);
        let thread = ThreadId::new();
        cx.update(|cx| {
            assert_eq!(holder(thread, cx), None);
            claim(thread, "claude", DEFAULT_CLAIM, cx).unwrap();
            assert_eq!(holder(thread, cx).as_deref(), Some("claude"));

            // The same client may renew; another may not take it.
            claim(thread, "claude", DEFAULT_CLAIM, cx).unwrap();
            let error = claim(thread, "other", DEFAULT_CLAIM, cx).unwrap_err();
            assert!(error.to_string().contains("claude"), "{error}");

            // Another client cannot release it; the person can.
            assert!(!release(thread, Some("other"), cx));
            assert_eq!(holder(thread, cx).as_deref(), Some("claude"));
            assert!(release(thread, None, cx));
            assert_eq!(holder(thread, cx), None);
            claim(thread, "other", DEFAULT_CLAIM, cx).unwrap();
        });
    }

    #[gpui::test]
    fn test_claims_are_per_thread(cx: &mut TestAppContext) {
        init_test(cx);
        let (first, second) = (ThreadId::new(), ThreadId::new());
        cx.update(|cx| {
            claim(first, "claude", DEFAULT_CLAIM, cx).unwrap();
            assert_eq!(holder(second, cx), None);
            claim(second, "other", DEFAULT_CLAIM, cx).unwrap();
            assert!(release(first, Some("claude"), cx));
            assert_eq!(holder(second, cx).as_deref(), Some("other"));
        });
    }

    #[gpui::test]
    async fn test_a_claim_ends_by_itself(cx: &mut TestAppContext) {
        init_test(cx);
        let thread = ThreadId::new();
        cx.update(|cx| claim(thread, "claude", Duration::from_secs(30), cx).unwrap());
        cx.executor().advance_clock(Duration::from_secs(10));
        cx.run_until_parked();
        assert_eq!(
            cx.update(|cx| holder(thread, cx)).as_deref(),
            Some("claude")
        );

        cx.executor().advance_clock(Duration::from_secs(25));
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| holder(thread, cx)), None, "it timed out");
        // And the next client can take it.
        cx.update(|cx| claim(thread, "other", DEFAULT_CLAIM, cx).unwrap());
    }

    #[gpui::test]
    async fn test_renewing_a_claim_restarts_its_clock(cx: &mut TestAppContext) {
        init_test(cx);
        let thread = ThreadId::new();
        cx.update(|cx| claim(thread, "claude", Duration::from_secs(30), cx).unwrap());
        cx.executor().advance_clock(Duration::from_secs(20));
        cx.run_until_parked();
        cx.update(|cx| claim(thread, "claude", Duration::from_secs(30), cx).unwrap());

        // The first timer fires here, but it belongs to the old claim.
        cx.executor().advance_clock(Duration::from_secs(15));
        cx.run_until_parked();
        assert_eq!(
            cx.update(|cx| holder(thread, cx)).as_deref(),
            Some("claude")
        );

        cx.executor().advance_clock(Duration::from_secs(20));
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| holder(thread, cx)), None);
    }

    #[gpui::test]
    async fn test_a_claim_cannot_last_longer_than_the_maximum(cx: &mut TestAppContext) {
        init_test(cx);
        let thread = ThreadId::new();
        cx.update(|cx| claim(thread, "claude", Duration::from_secs(60 * 60 * 24), cx).unwrap());
        cx.executor()
            .advance_clock(MAX_CLAIM + Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| holder(thread, cx)), None);
    }
}

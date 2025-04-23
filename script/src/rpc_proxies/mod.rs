use anyhow::Result;
use futures::future::{select_ok, BoxFuture};

pub mod consensus;
pub mod execution;

/// Takes a closure that produces a future for each provided client, waits for the first succeeding
/// future and returns its result
async fn multiplex<F, C, R>(f: F, clients: &[C]) -> Result<R>
where
    F: Fn(C) -> BoxFuture<'static, Result<R>>,
    C: Clone,
    R: 'static,
{
    // If no clients are provided, short circuit. It's required cause otherwise select_ok panics
    if clients.is_empty() {
        return Err(anyhow::anyhow!("No clients available"));
    }

    // For each client, create a future to run
    let futs = clients.iter().map(|client| f(client.clone()));

    // Return first succeeding future, or last error if all of them failed
    match select_ok(futs).await {
        Ok((value, _)) => Ok(value),
        Err(e) => Err(e),
    }
}

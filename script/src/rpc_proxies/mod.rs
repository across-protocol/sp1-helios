use std::{future::Future, pin::Pin};

use futures::future::select_ok;

pub mod consensus;
pub mod execution;

/// Takes a closure that produces a future for each provided client, waits for the first succeeding
/// future and returns its result
async fn multiplex<F, C, R>(f: F, clients: &[C]) -> anyhow::Result<R>
where
    F: Fn(C) -> Pin<Box<dyn Future<Output = anyhow::Result<R>>>>,
    // Clients are assumed to be very cheap to clone
    C: Clone + Sync + Send,
    R: 'static + Send,
{
    if clients.is_empty() {
        return Err(anyhow::anyhow!("No clients available"));
    }

    let mut futs = vec![];
    for client in clients {
        let fut = f(client.clone());
        futs.push(fut);
    }

    match select_ok(futs).await {
        Ok((value, _)) => Ok(value),
        Err(e) => Err(e),
    }
}

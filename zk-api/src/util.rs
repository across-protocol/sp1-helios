use tokio_util::sync::CancellationToken;

pub struct CancellationTokenGuard {
    cancellation_token: CancellationToken,
}

impl CancellationTokenGuard {
    pub fn new(cancellation_token: tokio_util::sync::CancellationToken) -> Self {
        Self { cancellation_token }
    }
}

impl Drop for CancellationTokenGuard {
    fn drop(&mut self) {
        self.cancellation_token.cancel();
    }
}

#[tokio::test]
async fn cancellation() {
    let token = CancellationToken::new();

    assert!(
        !token.is_cancelled(),
        "Token should not be cancelled initially"
    );

    // Create a guard and immediately drop it to trigger cancellation
    {
        let _guard = CancellationTokenGuard::new(token.clone());
    }

    assert!(
        token.is_cancelled(),
        "Token should be cancelled after guard is dropped"
    );

    token.cancelled().await;

    println!("Cancellation confirmed!");
}

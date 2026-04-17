use async_trait::async_trait;
use tokio::sync::broadcast;

#[async_trait]
pub trait PubSubProvider: Send + Sync {
    async fn publish(&self, payload: Vec<u8>);
    async fn subscribe(&self) -> Result<broadcast::Receiver<Vec<u8>>, String>;
}

pub struct LocalOnlyProvider;

#[async_trait]
impl PubSubProvider for LocalOnlyProvider {
    async fn publish(&self, _payload: Vec<u8>) {}

    async fn subscribe(&self) -> Result<broadcast::Receiver<Vec<u8>>, String> {
        let (_tx, rx) = broadcast::channel(1);
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_only_publish_is_noop() {
        let p = LocalOnlyProvider;
        p.publish(b"x".to_vec()).await;
    }

    #[tokio::test]
    async fn local_only_subscribe_is_empty() {
        let p = LocalOnlyProvider;
        let mut rx = p.subscribe().await.unwrap();
        assert!(rx.try_recv().is_err());
    }
}

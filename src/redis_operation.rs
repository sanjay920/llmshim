use redis::aio::ConnectionManager;
use std::future::Future;
use std::sync::{Arc, Mutex};

pub(crate) struct RedisConnectionManagerCache {
    client: redis::Client,
    current: Mutex<Option<Arc<ConnectionGeneration>>>,
    creation: tokio::sync::Mutex<()>,
}

struct ConnectionGeneration {
    manager: ConnectionManager,
}

impl RedisConnectionManagerCache {
    pub(crate) fn new(client: redis::Client) -> Self {
        Self {
            client,
            current: Mutex::new(None),
            creation: tokio::sync::Mutex::new(()),
        }
    }

    async fn generation(&self) -> redis::RedisResult<Arc<ConnectionGeneration>> {
        if let Some(generation) = self.current.lock().unwrap().as_ref().cloned() {
            return Ok(generation);
        }
        let _creation = self.creation.lock().await;
        if let Some(generation) = self.current.lock().unwrap().as_ref().cloned() {
            return Ok(generation);
        }
        let generation = Arc::new(ConnectionGeneration {
            manager: ConnectionManager::new(self.client.clone()).await?,
        });
        *self.current.lock().unwrap() = Some(generation.clone());
        Ok(generation)
    }

    pub(crate) async fn run<T, F, Fut>(&self, operation: F) -> redis::RedisResult<T>
    where
        F: FnOnce(ConnectionManager) -> Fut,
        Fut: Future<Output = redis::RedisResult<T>>,
    {
        let generation = self.generation().await?;
        let mut retirement = ConnectionRetirement {
            cache: self,
            generation: generation.clone(),
            armed: true,
        };
        let result = operation(generation.manager.clone()).await;
        if result.is_ok() {
            retirement.armed = false;
        }
        result
    }

    #[cfg(test)]
    pub(crate) async fn connection_for_test(&self) -> redis::RedisResult<ConnectionManager> {
        Ok(self.generation().await?.manager.clone())
    }

    fn retire(&self, generation: &Arc<ConnectionGeneration>) {
        let mut current = self.current.lock().unwrap();
        if current
            .as_ref()
            .is_some_and(|candidate| Arc::ptr_eq(candidate, generation))
        {
            current.take();
        }
    }
}

struct ConnectionRetirement<'a> {
    cache: &'a RedisConnectionManagerCache,
    generation: Arc<ConnectionGeneration>,
    armed: bool,
}

impl Drop for ConnectionRetirement<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.cache.retire(&self.generation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Notify;

    struct SyntheticPeer {
        url: String,
        accepts: Arc<AtomicUsize>,
        closed: Arc<AtomicUsize>,
        pings: Arc<AtomicUsize>,
        activity: Arc<Notify>,
        stall_ping: Arc<AtomicBool>,
        task: tokio::task::JoinHandle<()>,
    }

    impl SyntheticPeer {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let accepts = Arc::new(AtomicUsize::new(0));
            let closed = Arc::new(AtomicUsize::new(0));
            let pings = Arc::new(AtomicUsize::new(0));
            let activity = Arc::new(Notify::new());
            let stall_ping = Arc::new(AtomicBool::new(false));
            let server_accepts = accepts.clone();
            let server_closed = closed.clone();
            let server_pings = pings.clone();
            let server_activity = activity.clone();
            let server_stall_ping = stall_ping.clone();
            let task = tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    server_accepts.fetch_add(1, Ordering::SeqCst);
                    server_activity.notify_waiters();
                    let connection_closed = server_closed.clone();
                    let connection_pings = server_pings.clone();
                    let connection_activity = server_activity.clone();
                    let connection_stall_ping = server_stall_ping.clone();
                    tokio::spawn(async move {
                        let mut buffer = [0_u8; 8192];
                        loop {
                            let received = match socket.read(&mut buffer).await {
                                Ok(0) | Err(_) => {
                                    connection_closed.fetch_add(1, Ordering::SeqCst);
                                    connection_activity.notify_waiters();
                                    return;
                                }
                                Ok(received) => received,
                            };
                            let data = &buffer[..received];
                            let command_count = data
                                .iter()
                                .enumerate()
                                .filter(|(index, byte)| {
                                    **byte == b'*' && (*index == 0 || data[*index - 1] == b'\n')
                                })
                                .count();
                            let ping_count = String::from_utf8_lossy(data).matches("PING").count();
                            if ping_count > 0 {
                                connection_pings.fetch_add(ping_count, Ordering::SeqCst);
                                connection_activity.notify_waiters();
                            }
                            if ping_count > 0 && connection_stall_ping.load(Ordering::SeqCst) {
                                continue;
                            }
                            let reply: &[u8] = if ping_count > 0 {
                                b"+PONG\r\n"
                            } else {
                                b"+OK\r\n"
                            };
                            for _ in 0..command_count {
                                if socket.write_all(reply).await.is_err() {
                                    return;
                                }
                            }
                        }
                    });
                }
            });
            Self {
                url: format!("redis://{address}/"),
                accepts,
                closed,
                pings,
                activity,
                stall_ping,
                task,
            }
        }

        async fn wait_for(&self, counter: &AtomicUsize, minimum: usize) {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while counter.load(Ordering::SeqCst) < minimum {
                    self.activity.notified().await;
                }
            })
            .await
            .unwrap();
        }
    }

    impl Drop for SyntheticPeer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn ping(cache: &RedisConnectionManagerCache) -> redis::RedisResult<String> {
        cache
            .run(|mut connection| async move {
                redis::cmd("PING").query_async(&mut connection).await
            })
            .await
    }

    #[tokio::test]
    async fn healthy_calls_share_and_uncertain_calls_retire_their_generation() {
        let peer = SyntheticPeer::start().await;
        let cache = Arc::new(RedisConnectionManagerCache::new(
            redis::Client::open(peer.url.as_str()).unwrap(),
        ));

        let mut healthy = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            healthy.push(tokio::spawn(async move { ping(&cache).await.unwrap() }));
        }
        for call in healthy {
            assert_eq!(call.await.unwrap(), "PONG");
        }
        assert_eq!(peer.accepts.load(Ordering::SeqCst), 1);

        peer.stall_ping.store(true, Ordering::SeqCst);
        let pings_before_cancellation = peer.pings.load(Ordering::SeqCst);
        let canceled_cache = cache.clone();
        let canceled = tokio::spawn(async move { ping(&canceled_cache).await });
        peer.wait_for(&peer.pings, pings_before_cancellation + 1)
            .await;
        canceled.abort();
        let _ = canceled.await;
        peer.wait_for(&peer.closed, 1).await;
        assert_eq!(
            peer.pings.load(Ordering::SeqCst),
            pings_before_cancellation + 1
        );

        peer.stall_ping.store(false, Ordering::SeqCst);
        assert_eq!(ping(&cache).await.unwrap(), "PONG");
        assert_eq!(peer.accepts.load(Ordering::SeqCst), 2);

        peer.stall_ping.store(true, Ordering::SeqCst);
        let pings_before_timeout = peer.pings.load(Ordering::SeqCst);
        let started = std::time::Instant::now();
        assert!(ping(&cache).await.is_err());
        assert!(started.elapsed() >= std::time::Duration::from_millis(450));
        peer.wait_for(&peer.closed, 2).await;
        assert_eq!(peer.pings.load(Ordering::SeqCst), pings_before_timeout + 1);

        peer.stall_ping.store(false, Ordering::SeqCst);
        assert_eq!(ping(&cache).await.unwrap(), "PONG");
        assert_eq!(peer.accepts.load(Ordering::SeqCst), 3);
    }
}

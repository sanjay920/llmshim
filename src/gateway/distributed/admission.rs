use redis::aio::ConnectionManager;
use std::sync::LazyLock;

static ENQUEUE: LazyLock<redis::Script> = LazyLock::new(|| {
    redis::Script::new(
        r#"
        local waiting_depth = redis.call('ZCARD', KEYS[1])
            + redis.call('ZCARD', KEYS[2])
            + redis.call('ZCARD', KEYS[3])
            + redis.call('ZCARD', KEYS[4])
        local maximum_depth = tonumber(ARGV[1])
        if waiting_depth >= maximum_depth then
            return 0
        end
        redis.call('ZADD', KEYS[1], ARGV[2], ARGV[3])
        return 1
        "#,
    )
});

pub(super) async fn enqueue(
    connection: &mut ConnectionManager,
    queue_keys: [&str; 4],
    maximum_depth: usize,
    priority_score: f64,
    serialized_member: &str,
) -> redis::RedisResult<bool> {
    let admitted: i64 = ENQUEUE
        .key(queue_keys[0])
        .key(queue_keys[1])
        .key(queue_keys[2])
        .key(queue_keys[3])
        .arg(maximum_depth)
        .arg(priority_score)
        .arg(serialized_member)
        .invoke_async(connection)
        .await?;
    Ok(admitted == 1)
}

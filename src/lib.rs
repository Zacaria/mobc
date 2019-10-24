mod config;

use config::Builder;
use config::Config;
pub use futures;
pub use futures::compat::Future01CompatExt;
pub use futures::compat::Stream01CompatExt;
pub use futures::Future;
pub use futures::FutureExt;
use std::error;
use std::fmt;
use std::marker::Unpin;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio_executor::Executor as TkExecutor;

static CONNECTION_ID: AtomicUsize = AtomicUsize::new(0);

pub enum Error<E> {
    Inner(E),
    Timeout,
}

impl<E> From<E> for Error<E> {
    fn from(e: E) -> Error<E> {
        Error::Inner(e)
    }
}

pub trait Executor: TkExecutor + Send + Sync + 'static + Clone {}

pub type AnyFuture<T, E> = Box<dyn Future<Output = Result<T, E>> + Unpin + Send>;

pub trait ConnectionManager: Send + Sync + 'static {
    type Connection: Send + 'static;
    type Error: error::Error + Send + 'static;
    type Executor: TkExecutor + Send + Sync + 'static + Clone;

    fn get_executor(&self) -> Self::Executor;
    fn connect(&self) -> AnyFuture<Self::Connection, Self::Error>;
    fn is_valid(&self, conn: Self::Connection) -> AnyFuture<Self::Connection, Self::Error>;
    fn has_broken(&self, conn: &mut Self::Connection) -> bool;
}

struct Conn<C> {
    raw: Option<C>,
    id: u64,
    birth: Instant,
}

struct IdleConn<C> {
    conn: Conn<C>,
    idle_start: Instant,
}

struct PoolInternals<C> {
    conns: Vec<IdleConn<C>>,
    num_conns: u32,
    pending_conns: u32,
    last_error: Option<String>,
}

struct SharedPool<M>
where
    M: ConnectionManager,
{
    config: Config<M::Executor>,
    manager: M,
    internals: Mutex<PoolInternals<M::Connection>>,
}

/// A generic connection pool.
pub struct Pool<M>(Arc<SharedPool<M>>)
where
    M: ConnectionManager;

/// Returns a new `Pool` referencing the same state as `self`.
impl<M> Clone for Pool<M>
where
    M: ConnectionManager,
{
    fn clone(&self) -> Self {
        Pool(self.0.clone())
    }
}

impl<M> Pool<M>
where
    M: ConnectionManager,
{
    /// Creates a new connection pool with a default configuration.
    pub async fn new<E>(manager: M) -> Result<Pool<M>, Error<E>>
    where
        Error<E>: std::convert::From<<M as ConnectionManager>::Error>,
    {
        Pool::builder().build(manager).await
    }

    /// Returns a builder type to configure a new pool.
    pub fn builder() -> Builder<M> {
        Builder::new()
    }

    pub fn new_inner(config: Config<M::Executor>, manager: M) -> Pool<M> {
        let internals = PoolInternals {
            conns: Vec::with_capacity(config.max_size as usize),
            num_conns: 0,
            pending_conns: 0,
            last_error: None,
        };

        let shared = Arc::new(SharedPool {
            config: config,
            manager: manager,
            internals: Mutex::new(internals),
        });

        establish_idle_connections(&shared, &mut shared.internals.lock().unwrap());

        Pool(shared)
    }

    /// Retrieves a connection from the pool.
    ///
    /// Waits for at most the configured connection timeout before returning an
    /// error.
    pub async fn get<E>(&self) -> Result<PooledConnection<M>, Error<E>>
    where
        Error<E>: std::convert::From<<M as ConnectionManager>::Error>,
    {
        self.get_timeout(self.0.config.connection_timeout).await
    }

    /// Retrieves a connection from the pool, waiting for at most `timeout`
    ///
    /// The given timeout will be used instead of the configured connection
    /// timeout.
    pub async fn get_timeout<E>(&self, timeout: Duration) -> Result<PooledConnection<M>, Error<E>>
    where
        Error<E>: std::convert::From<<M as ConnectionManager>::Error>,
    {
        let start = Instant::now();
        let end = start + timeout;

        loop {
            match self.try_get_inner().await {
                Ok(conn) => {
                    return Ok(conn);
                }
                Err(_) => (),
            }
            {
                let mut internals = self.0.internals.lock().unwrap();
                add_connection(&self.0, &mut internals);
            }
            
        }
    }

    async fn try_get_inner(&self) -> Result<PooledConnection<M>, ()> {
        loop {
            let mut internals = self.0.internals.lock().unwrap();
            if let Some(mut conn) = internals.conns.pop() {
                establish_idle_connections(&self.0, &mut internals);
                drop(internals);

                return Ok(PooledConnection {
                    pool: self.clone(),
                    conn: conn.conn,
                });
            } else {
                return Err(());
            }
        }
    }

    async fn wait_for_initialization<E>(&self) -> Result<(), Error<E>>
    where
        Error<E>: std::convert::From<<M as ConnectionManager>::Error>,
    {
        Ok(())
    }
}

fn establish_idle_connections<M>(
    shared: &Arc<SharedPool<M>>,
    internals: &mut PoolInternals<M::Connection>,
) where
    M: ConnectionManager,
{
    let min = shared.config.min_idle.unwrap_or(shared.config.max_size);
    let idle = internals.conns.len() as u32;
    for _ in idle..min {
        add_connection(shared, internals);
    }
}

fn add_connection<M>(shared: &Arc<SharedPool<M>>, internals: &mut PoolInternals<M::Connection>)
where
    M: ConnectionManager,
{
    if internals.num_conns + internals.pending_conns >= shared.config.max_size {
        return;
    }

    internals.pending_conns += 1;
    inner(Duration::from_secs(0), shared);

    fn inner<M>(delay: Duration, shared: &Arc<SharedPool<M>>)
    where
        M: ConnectionManager,
    {
        let new_shared = Arc::downgrade(shared);
        shared.config.executor.clone().spawn(Box::pin(async move {
            let shared = match new_shared.upgrade() {
                Some(shared) => shared,
                None => return,
            };

            let conn = shared.manager.connect().await;
            match conn {
                Ok(conn) => {
                    let id = CONNECTION_ID.fetch_add(1, Ordering::Relaxed) as u64;
                    let mut internals = shared.internals.lock().unwrap();
                    internals.last_error = None;
                    let now = Instant::now();
                    let conn = IdleConn {
                        conn: Conn {
                            raw: Some(conn),
                            birth: now,
                            id,
                        },
                        idle_start: now,
                    };

                    internals.conns.push(conn);
                    internals.pending_conns -= 1;
                    internals.num_conns += 1;
                    // todo notify the wait
                }
                Err(err) => {
                    shared.internals.lock().unwrap().last_error = Some(err.to_string());
                    let delay = Duration::from_millis(200);
                    inner(delay, &shared);
                }
            }
        }));
    }
}

pub struct PooledConnection<M>
where
    M: ConnectionManager,
{
    pool: Pool<M>,
    conn: Conn<M::Connection>,
}

impl<M> PooledConnection<M> 
where
    M: ConnectionManager,
{
    pub fn take_raw_conn(&mut self) -> M::Connection {
        self.conn.raw.take().unwrap()
    }

    pub fn set_raw_conn(&mut self, raw: M::Connection)  {
        self.conn.raw = Some(raw);
    }
}

impl<M> Drop for PooledConnection<M>
where
    M: ConnectionManager,
{
    fn drop(&mut self) {
        println!("drop2");
    }
}

impl<M> Deref for PooledConnection<M>
where
    M: ConnectionManager,
{
    type Target = M::Connection;
    fn deref(&self) -> &Self::Target {
        &self.conn.raw.as_ref().unwrap()
    }
}

impl<M> DerefMut for PooledConnection<M>
where
    M: ConnectionManager,
{
    fn deref_mut(&mut self) -> &mut M::Connection {
        self.conn.raw.as_mut().unwrap()
    }
}
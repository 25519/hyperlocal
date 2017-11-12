//! Hyper server bindings for unix domain sockets

use std::io;
use std::marker::PhantomData;
use std::path::Path;
use std::rc::{Rc, Weak};
use std::cell::RefCell;
use std::time::Duration;

use futures::{Async, Poll};
use futures::task::{self, Task};
use futures::future::{self, Future};
use futures::stream::Stream;
use hyper::{Request, Response};
use hyper::server::Http as HyperHttp;
use tokio_core::reactor::{Core, Timeout};
use tokio_service::{NewService, Service};
use tokio_uds::UnixListener;

/// An instance of a server created through `Http::bind`.
//
/// This structure is used to create instances of Servers to spawn off tasks
/// which handle a connection to an HTTP server.
pub struct Server<S, B>
where
    B: Stream<Error = ::hyper::Error>,
    B::Item: AsRef<[u8]>,
{
    protocol: HyperHttp<B::Item>,
    new_service: S,
    reactor: Core,
    listener: UnixListener,
    shutdown_timeout: Duration,
}

impl<S, B> Server<S, B>
where
    S: NewService<Request = Request, Response = Response<B>, Error = ::hyper::Error>
        + Send
        + Sync
        + 'static,
    B: Stream<Error = ::hyper::Error> + 'static,
    B::Item: AsRef<[u8]>,
{
    pub fn run(self) -> ::hyper::Result<()> {
        self.run_until(future::empty())
    }

    pub fn run_until<F>(self, shutdown_signal: F) -> ::hyper::Result<()>
    where
        F: Future<Item = (), Error = ()>,
    {
        let Server {
            protocol,
            new_service,
            mut reactor,
            listener,
            shutdown_timeout,
        } = self;
        let handle = reactor.handle();

        let info = Rc::new(RefCell::new(Info {
            active: 0,
            blocker: None,
        }));

        let srv = listener.incoming().for_each(|(socket, _)| {
            let s = NotifyService {
                inner: new_service.new_service()?,
                info: Rc::downgrade(&info),
            };
            info.borrow_mut().active += 1;
            protocol.bind_connection(&handle, socket, ([127, 0, 0, 1], 0).into(), s);
            Ok(())
        });

        let shutdown_signal = shutdown_signal.then(|_| Ok(()));

        match reactor.run(shutdown_signal.select(srv)) {
            Ok(((), _incoming)) => {}
            Err((e, _other)) => return Err(e.into()),
        }

        let timeout = Timeout::new(shutdown_timeout, &handle)?;
        let wait = WaitUntilZero { info: info.clone() };
        match reactor.run(wait.select(timeout)) {
            Ok(_) => Ok(()),
            Err((e, _)) => Err(e.into()),
        }
    }
}

struct NotifyService<S> {
    inner: S,
    info: Weak<RefCell<Info>>,
}

struct WaitUntilZero {
    info: Rc<RefCell<Info>>,
}

struct Info {
    active: usize,
    blocker: Option<Task>,
}

impl<S: Service> Service for NotifyService<S> {
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn call(&self, message: Self::Request) -> Self::Future {
        self.inner.call(message)
    }
}

impl<S> Drop for NotifyService<S> {
    fn drop(&mut self) {
        let info = match self.info.upgrade() {
            Some(info) => info,
            None => return,
        };
        let mut info = info.borrow_mut();
        info.active -= 1;
        if info.active == 0 {
            if let Some(task) = info.blocker.take() {
                task.notify();
            }
        }
    }
}

impl Future for WaitUntilZero {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), io::Error> {
        let mut info = self.info.borrow_mut();
        if info.active == 0 {
            Ok(().into())
        } else {
            info.blocker = Some(task::current());
            Ok(Async::NotReady)
        }
    }
}

/// A type that provides a factory interface for creating
/// unix socket based Servers
///
/// # examples
///
/// ```no_run
/// extern crate hyper;
/// extern crate hyperlocal;
///
/// //let server = hyperlocal::Http::new().bind(
///  // "path/to/socket",
///  //  || Ok(HelloWorld)
/// //)
///
/// ```
pub struct Http<B = ::hyper::Chunk> {
    _marker: PhantomData<B>,
}

impl<B> Clone for Http<B> {
    fn clone(&self) -> Http<B> {
        Http { ..*self }
    }
}

impl<B: AsRef<[u8]> + 'static> Http<B> {
    /// Creates a new instance of the HTTP protocol, ready to spawn a server or
    /// start accepting connections.
    pub fn new() -> Http<B> {
        Http { _marker: PhantomData }
    }

    /// binds a new server instance to a unix domain socket path
    pub fn bind<P, S, Bd>(&self, path: P, new_service: S) -> ::hyper::Result<Server<S, Bd>>
    where
        P: AsRef<Path>,
        S: NewService<Request = Request, Response = Response<Bd>, Error = ::hyper::Error>
            + Send
            + Sync
            + 'static,
        Bd: Stream<Item = B, Error = ::hyper::Error> + 'static,
    {
        let core = Core::new()?;
        let handle = core.handle();
        let listener = UnixListener::bind(path.as_ref(), &handle)?;

        Ok(Server {
            protocol: HyperHttp::new(),
            new_service: new_service,
            reactor: core,
            listener: listener,
            shutdown_timeout: Duration::new(1, 0),
        })
    }
}

use std::io::{self, ErrorKind, Read, Write};
use std::sync::Arc;
use std::net::{self, SocketAddr};

use futures::io::Ready;
use futures::stream::{self, Stream};
use futures::{Future, IntoFuture, failed, Task, Poll};
use mio;

use {IoFuture, IoStream, ReadinessStream, LoopHandle};

/// An I/O object representing a TCP socket listening for incoming connections.
///
/// This object can be converted into a stream of incoming connections for
/// various forms of processing.
pub struct TcpListener {
    loop_handle: LoopHandle,
    ready: ReadinessStream,
    listener: Arc<mio::tcp::TcpListener>,
}

impl TcpListener {
    fn new(listener: mio::tcp::TcpListener,
           handle: LoopHandle) -> Box<IoFuture<TcpListener>> {
        let listener = Arc::new(listener);
        ReadinessStream::new(handle.clone(), listener.clone()).map(|r| {
            TcpListener {
                loop_handle: handle,
                ready: r,
                listener: listener,
            }
        }).boxed()
    }

    /// Create a new TCP listener from the standard library's TCP listener.
    ///
    /// This method can be used when the `LoopHandle::tcp_listen` method isn't
    /// sufficient because perhaps some more configuration is needed in terms of
    /// before the calls to `bind` and `listen`.
    ///
    /// This API is typically paired with the `net2` crate and the `TcpBuilder`
    /// type to build up and customize a listener before it's shipped off to the
    /// backing event loop. This allows configuration of options like
    /// `SO_REUSEPORT`, binding to multiple addresses, etc.
    ///
    /// The `addr` argument here is one of the addresses that `listener` is
    /// bound to and the listener will only be guaranteed to accept connections
    /// of the same address type currently.
    ///
    /// Finally, the `handle` argument is the event loop that this listener will
    /// be bound to.
    ///
    /// The platform specific behavior of this function looks like:
    ///
    /// * On Unix, the socket is placed into nonblocking mode and connections
    ///   can be accepted as normal
    ///
    /// * On Windows, the address is stored internally and all future accepts
    ///   will only be for the same IP version as `addr` specified. That is, if
    ///   `addr` is an IPv4 address then all sockets accepted will be IPv4 as
    ///   well (same for IPv6).
    pub fn from_listener(listener: net::TcpListener,
                         addr: &SocketAddr,
                         handle: LoopHandle) -> Box<IoFuture<TcpListener>> {
        mio::tcp::TcpListener::from_listener(listener, addr)
            .into_future()
            .and_then(|l| TcpListener::new(l, handle))
            .boxed()
    }

    /// Returns the local address that this listener is bound to.
    ///
    /// This can be useful, for example, when binding to port 0 to figure out
    /// which port was actually bound.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Consumes this listener, returning a stream of the sockets this listener
    /// accepts.
    ///
    /// This method returns an implementation of the `Stream` trait which
    /// resolves to the sockets the are accepted on this listener.
    pub fn incoming(self) -> Box<IoStream<(TcpStream, SocketAddr)>> {
        let TcpListener { loop_handle, listener, ready } = self;

        ready
            .map(move |_| {
                stream::iter(NonblockingIter { source: listener.clone() }.fuse())
            })
            .flatten()
            .and_then(move |(tcp, addr)| {
                let tcp = Arc::new(tcp);
                ReadinessStream::new(loop_handle.clone(),
                                     tcp.clone()).map(move |ready| {
                    let stream = TcpStream {
                        source: tcp,
                        ready: ready,
                    };
                    (stream, addr)
                })
            }).boxed()
    }
}

struct NonblockingIter {
    source: Arc<mio::tcp::TcpListener>,
}

impl Iterator for NonblockingIter {
    type Item = io::Result<(mio::tcp::TcpStream, SocketAddr)>;

    fn next(&mut self) -> Option<io::Result<(mio::tcp::TcpStream, SocketAddr)>> {
        match self.source.accept() {
            Ok(Some(e)) => {
                debug!("accepted connection");
                Some(Ok(e))
            }
            Ok(None) => {
                debug!("no connection ready");
                None
            }
            Err(e) => Some(Err(e)),
        }
    }
}

impl Stream for TcpListener {
    type Item = Ready;
    type Error = io::Error;

    fn poll(&mut self, task: &mut Task) -> Poll<Option<Ready>, io::Error> {
        self.ready.poll(task)
    }

    fn schedule(&mut self, task: &mut Task) {
        self.ready.schedule(task)
    }
}

/// An I/O object representing a TCP stream connected to a remote endpoint.
///
/// A TCP stream can either be created by connecting to an endpoint or by
/// accepting a connection from a listener. Inside the stream is access to the
/// raw underlying I/O object as well as streams for the read/write
/// notifications on the stream itself.
pub struct TcpStream {
    source: Arc<mio::tcp::TcpStream>,
    ready: ReadinessStream,
}

impl LoopHandle {
    /// Create a new TCP listener associated with this event loop.
    ///
    /// The TCP listener will bind to the provided `addr` address, if available,
    /// and will be returned as a future. The returned future, if resolved
    /// successfully, can then be used to accept incoming connections.
    pub fn tcp_listen(self, addr: &SocketAddr) -> Box<IoFuture<TcpListener>> {
        match mio::tcp::TcpListener::bind(addr) {
            Ok(l) => TcpListener::new(l, self),
            Err(e) => failed(e).boxed(),
        }
    }

    /// Create a new TCP stream connected to the specified address.
    ///
    /// This function will create a new TCP socket and attempt to connect it to
    /// the `addr` provided. The returned future will be resolved once the
    /// stream has successfully connected. If an error happens during the
    /// connection or during the socket creation, that error will be returned to
    /// the future instead.
    pub fn tcp_connect(self, addr: &SocketAddr) -> Box<IoFuture<TcpStream>> {
        match mio::tcp::TcpStream::connect(addr) {
            Ok(tcp) => TcpStream::new(tcp, self),
            Err(e) => failed(e).boxed(),
        }
    }
}

impl TcpStream {
    fn new(connected_stream: mio::tcp::TcpStream,
           handle: LoopHandle)
           -> Box<IoFuture<TcpStream>> {
        // Once we've connected, wait for the stream to be writable as that's
        // when the actual connection has been initiated. Once we're writable we
        // check for `take_socket_error` to see if the connect actually hit an
        // error or not.
        //
        // If all that succeeded then we ship everything on up.
        let connected_stream = Arc::new(connected_stream);
        ReadinessStream::new(handle, connected_stream.clone()).and_then(|ready| {
            let source = connected_stream.clone();
            let connected = ready.skip_while(move |&_| {
                match source.take_socket_error() {
                    Ok(()) => Ok(false),
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => Ok(true),
                    Err(e) => Err(e),
                }
            });
            let connected = connected.into_future();
            connected.map(move |(_, ready)| {
                TcpStream {
                    source: connected_stream,
                    ready: ready.into_inner(),
                }
            }).map_err(|(e, _)| e)
        }).boxed()
    }

    /// Creates a new `TcpStream` from the pending socket inside the given
    /// `std::net::TcpStream`, connecting it to the address specified.
    ///
    /// This constructor allows configuring the socket before it's actually
    /// connected, and this function will transfer ownership to the returned
    /// `TcpStream` if successful. An unconnected `TcpStream` can be created
    /// with the `net2::TcpBuilder` type (and also configured via that route).
    ///
    /// The platform specific behavior of this function looks like:
    ///
    /// * On Unix, the socket is placed into nonblocking mode and then a
    ///   `connect` call is issued.
    ///
    /// * On Windows, the address is stored internally and the connect operation
    ///   is issued when the returned `TcpStream` is registered with an event
    ///   loop. Note that on Windows you must `bind` a socket before it can be
    ///   connected, so if a custom `TcpBuilder` is used it should be bound
    ///   (perhaps to `INADDR_ANY`) before this method is called.
    pub fn connect_stream(stream: net::TcpStream,
                          addr: &SocketAddr,
                          handle: LoopHandle) -> Box<IoFuture<TcpStream>> {
        match mio::tcp::TcpStream::connect_stream(stream, addr) {
            Ok(tcp) => TcpStream::new(tcp, handle),
            Err(e) => failed(e).boxed(),
        }
    }

    /// Returns the local address that this stream is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.source.local_addr()
    }

    /// Returns the remote address that this stream is connected to.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.source.peer_addr()
    }
}

impl Read for TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self.source).read(buf)
    }
}

impl Write for TcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self.source).write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        (&*self.source).flush()
    }
}

impl<'a> Read for &'a TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self.source).read(buf)
    }
}

impl<'a> Write for &'a TcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self.source).write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        (&*self.source).flush()
    }
}

impl Stream for TcpStream {
    type Item = Ready;
    type Error = io::Error;

    fn poll(&mut self, task: &mut Task) -> Poll<Option<Ready>, io::Error> {
        self.ready.poll(task)
    }

    fn schedule(&mut self, task: &mut Task) {
        self.ready.schedule(task)
    }
}

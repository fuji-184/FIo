use std::io;
use std::mem::MaybeUninit;
use std::process::Command;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use tokio::net::{TcpSocket, TcpStream};

#[cfg(any(feature = "io_uring_registry", feature = "io_uring_pool"))]
use tokio_uring::buf::fixed::FixedBufRegistry;

#[cfg(any(feature = "work_stealing", feature = "share_nothing"))]
use crate::request::{self, Request};
#[cfg(any(feature = "work_stealing", feature = "share_nothing"))]
use crate::response::{self, Response};

#[cfg(any(feature = "io_uring_registry", feature = "io_uring_pool"))]
use httparse::{EMPTY_HEADER, Request, Status};

#[cfg(any(feature = "io_uring_registry", feature = "io_uring_pool"))]
use std::fmt::Write;

#[cfg(any(feature = "io_uring_registry", feature = "io_uring_pool"))]
use std::iter;

#[cfg(any(feature = "io_uring_registry", feature = "io_uring_pool"))]
use tokio_uring::buf::fixed::FixedBufPool;

#[cfg(any(feature = "io_uring_registry", feature = "io_uring_pool"))]
const BUF_SIZE: usize = 1024 * 8;

#[cfg(any(feature = "io_uring_registry", feature = "io_uring_pool"))]
const POOL_SIZE: usize = 1024;

#[cfg(any(feature = "io_uring_registry", feature = "io_uring_pool"))]
use crate::io_uring::{Res, *};

pub trait HttpService {
    #[cfg(feature = "work_stealing")]
    fn router(
        &mut self,
        req: Request,
        rsp: &mut Response,
    ) -> impl std::future::Future<Output = io::Result<()>> + Send;

    #[cfg(feature = "share_nothing")]
    async fn router(&mut self, req: Request, rsp: &mut Response) -> io::Result<()>;

    #[cfg(any(feature = "io_uring_registry", feature = "io_uring_pool"))]
    fn router(&self, req: Request) -> Res;
}

pub trait HttpServiceFactory: Send + Sized + 'static {
    type Service: HttpService + Send + Clone;
    fn new_service(&self) -> Self::Service;

    fn start(self, addr: &str) {
        #[cfg(feature = "work_stealing")]
        self.work_stealing(addr);

        #[cfg(feature = "share_nothing")]
        self.share_nothing(addr);

        #[cfg(feature = "io_uring_registry")]
        self.io_uring_registry(addr);

        #[cfg(feature = "io_uring_pool")]
        self.io_uring_pool(addr);
    }

    #[cfg(feature = "work_stealing")]
    fn work_stealing(self, addr: &str) {
        let addr = addr.to_string();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async move {
            let service = self.new_service();
            let value = service.clone();
            let socket = TcpSocket::new_v4().unwrap();
            socket.bind(addr.parse().unwrap()).unwrap();

            let listener = socket.listen(get_somaxconn().unwrap()).unwrap();

            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let service = value.clone();
                tokio::spawn(async move {
                    if let Err(_) = each_connection_loop(stream, service).await {
                        //eprintln!("service err = {e:?}");
                    }
                });
            }
        });
    }

    #[cfg(feature = "share_nothing")]
    fn share_nothing(self, addr: &str) {
        let n = num_cpus::get_physical();

        let handles: Vec<_> = (0..n)
            .map(|_| {
                let addr = addr.to_string();
                let service = self.new_service();

                let value = service.clone();
                std::thread::spawn(move || {
                    let service = value.clone();

                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let local = tokio::task::LocalSet::new();

                    rt.block_on(local.run_until(async {
                        let socket = create_reuse_port_listener(&addr).unwrap();
                        let listener = socket.listen(get_somaxconn().unwrap()).unwrap();

                        loop {
                            let (stream, _) = listener.accept().await.unwrap();
                            let service = service.clone();
                            local.spawn_local(async move {
                                if let Err(_) = each_connection_loop(stream, service).await {
                                    //eprintln!("service err = {e:?}");
                                }
                            });
                        }
                    }))
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[cfg(feature = "io_uring_registry")]
    fn io_uring_registry(self, addr: &str) {
        let n = num_cpus::get_physical();
        let service = self.new_service();
        let handles: Vec<_> = (0..n)
            .map(|_| {
                let addr = addr.to_string();
                let value = service.clone();
                std::thread::spawn(move || {
                    let registry = FixedBufRegistry::new(
                        iter::repeat_with(|| Vec::with_capacity(BUF_SIZE)).take(POOL_SIZE),
                    );

                    let response_pool = std::rc::Rc::new(std::cell::RefCell::new(
                        (0..1000)
                            .map(|_| String::with_capacity(POOL_SIZE))
                            .collect::<Vec<_>>(),
                    ));
                    let service = value.clone();
                    tokio_uring::start(async move {
                        registry.register().expect("register failed");

                        let socket = create_reuse_port_listener(&addr).unwrap();
                        let listener = socket.listen(get_somaxconn().unwrap()).unwrap();
                        let listener = tokio_uring::net::TcpListener::from_std(listener.into_std().unwrap());

                        loop {
                            let (stream, _) = listener.accept().await.unwrap();
                            let registry = registry.clone();
                            let pool2 = response_pool.clone();
                            let service = service.clone();
                            tokio_uring::spawn(async move {
                                let mut index = 0;
                                let mut buf = None;
                                while index < POOL_SIZE {
                                    if let Some(b) = registry.check_out(index) {
                                        buf = Some(b);
                                        break;
                                    }
                                    index += 1;
                                }

                                let mut buf = match buf {
                                    Some(b) => b,
                                    None => return,
                                };

                                let mut response_buf = pool2
                                    .borrow_mut()
                                    .pop()
                                    .unwrap_or_else(|| String::with_capacity(1024));

                                let mut read_pos = 0;
                                let mut write_pos = 0;

                                loop {
                                    let (res, buf_back) = stream.read(buf).await;
                                    buf = buf_back;

                                    let n = match res {
                                        Ok(0) => return,
                                        Ok(n) => n,
                                        Err(_) => return,
                                    };

                                    let slice = &buf[..n];
                                    write_pos = n;

                                    let mut headers = [EMPTY_HEADER; 32];
                                    let mut req = Request::new(&mut headers);
                                    match req.parse(&slice[..write_pos]) {
                                        Ok(Status::Complete(_used)) => {
                                            let mut should_close = false;
                                            for header in req.headers.iter() {
                                                if header.name.eq_ignore_ascii_case("connection") {
                                                    if let Ok(val) = str::from_utf8(header.value) {
                                                        if val
                                                            .to_ascii_lowercase()
                                                            .contains("close")
                                                        {
                                                            should_close = true;
                                                        }
                                                    }
                                                }
                                            }

                                            let response = service.router(req);
                                            let connection_header = if should_close {
                                                "Connection: close".to_string()
                                            } else {
                                                "Connection: keep-alive".to_string()
                                            };

                                            response_buf.clear();
                                            let _ = write!(
                                                response_buf,
                                                "HTTP/1.1 {} OK\r\nContent-Length: {}\r\nContent-Type: {}\r\n",
                                                response.status_code,
                                                response.body.len(),
                                                response.content_type,
                                            );

                                            for (k, v) in response.headers {
                                                let _ = write!(response_buf, "{}: {}\r\n", k, v);
                                            }

                                            let _ = write!(
                                                response_buf,
                                                "{}\r\n\r\n{}",
                                                connection_header, response.body
                                            );
                                            let (res, _) = stream
                                                .write_all(response_buf.as_bytes().to_vec())
                                                .await;
                                            if res.is_err() || should_close {
                                                return;
                                            }
                                        }
                                        _ => return,
                                    }
                                }

                                pool2.borrow_mut().push(response_buf);
                            });
                        }
                    });
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[cfg(feature = "io_uring_pool")]
    fn io_uring_pool(self, addr: &str) {
        let n = num_cpus::get_physical();
        let service = self.new_service();
        let value = service.clone();

        let handles: Vec<_> = (0..n)
            .map(|_| {
                let addr = addr.to_string();
                let value = value.clone();
                std::thread::spawn(move || {
                    let pool = FixedBufPool::new(
                        iter::repeat_with(|| Vec::with_capacity(BUF_SIZE)).take(POOL_SIZE),
                    );
                    let response_pool = std::rc::Rc::new(std::cell::RefCell::new(
                        (0..1000)
                            .map(|_| String::with_capacity(POOL_SIZE))
                            .collect::<Vec<_>>(),
                    ));

                    let service = value.clone();

                    tokio_uring::start(async move {
                        pool.register().expect("register failed");

                        let socket = create_reuse_port_listener(&addr).unwrap();
                        let listener = socket.listen(get_somaxconn().unwrap()).unwrap();
                        let listener = tokio_uring::net::TcpListener::from_std(listener.into_std().unwrap());

                        loop {
                            let (stream, _) = listener.accept().await.unwrap();
                            let pool = pool.clone();
                            let pool2 = response_pool.clone();
                            let service = service.clone();

                            tokio_uring::spawn(async move {
                                let mut buf = match pool.try_next(BUF_SIZE) {
                                    Some(b) => b,
                                    None => return,
                                };

                                let mut response_buf = pool2
                                    .borrow_mut()
                                    .pop()
                                    .unwrap_or_else(|| String::with_capacity(POOL_SIZE));
                                let mut read_pos = 0;
                                let mut write_pos = 0;

                                loop {
                                    let (res, buf_back) = stream.read(buf).await;
                                    buf = buf_back;

                                    let n = match res {
                                        Ok(0) => return,
                                        Ok(n) => n,
                                        Err(_) => return,
                                    };

                                    let slice = &buf[..n];
                                    write_pos = n;

                                    let mut headers = [EMPTY_HEADER; 32];
                                    let mut req = Request::new(&mut headers);
                                    match req.parse(&slice[..write_pos]) {
                                        Ok(Status::Complete(_used)) => {
                                            let mut should_close = false;
                                            for header in req.headers.iter() {
                                                if header.name.eq_ignore_ascii_case("connection") {
                                                    if let Ok(val) = str::from_utf8(header.value) {
                                                        if val
                                                            .to_ascii_lowercase()
                                                            .contains("close")
                                                        {
                                                            should_close = true;
                                                        }
                                                    }
                                                }
                                            }

                                            let response = service.router(req);
                                            let connection_header = if should_close {
                                                "Connection: close".to_string()
                                            } else {
                                                "Connection: keep-alive".to_string()
                                            };

                                            response_buf.clear();
                                            let _ = write!(
                                                response_buf,
                                                "HTTP/1.1 {} OK\r\nContent-Length: {}\r\nContent-Type: {}\r\n",
                                                response.status_code,
                                                response.body.len(),
                                                response.content_type,
                                            );


                                            for (k, v) in response.headers {
                                                let _ = write!(response_buf, "{}: {}\r\n", k, v);
                                            }

                                            let _ = write!(
                                                response_buf,
                                                "{}\r\n\r\n{}",
                                                connection_header, response.body
                                            );
                                            let (res, _) = stream
                                                .write_all(response_buf.as_bytes().to_vec())
                                                .await;
                                            if res.is_err() || should_close {
                                                return;
                                            }
                                        }
                                        _ => return,
                                    }
                                }

                                pool2.borrow_mut().push(response_buf);
                            });
                        }
                    });
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }
}

pub struct HttpServer<T>(pub T);

impl<T: HttpService + Clone + Send + Sync + 'static> HttpServer<T> {
    pub fn start(self, addr: &str) {
        #[cfg(feature = "work_stealing")]
        self.work_stealing(addr);

        #[cfg(feature = "share_nothing")]
        self.share_nothing(addr);

        #[cfg(feature = "io_uring_registry")]
        self.io_uring_registry(addr);

        #[cfg(feature = "io_uring_pool")]
        self.io_uring_pool(addr);
    }

    #[cfg(feature = "work_stealing")]
    pub fn work_stealing(self, addr: &str) {
        let addr = addr.to_string();
        let service = self.0;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async move {
            let value = service.clone();

            let socket = TcpSocket::new_v4().unwrap();
            socket.bind(addr.parse().unwrap()).unwrap();
            let listener = socket.listen(get_somaxconn().unwrap()).unwrap();

            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let service = value.clone();
                tokio::spawn(async move {
                    if let Err(_) = each_connection_loop(stream, service).await {
                        //eprintln!("service err = {e:?}");
                    }
                });
            }
        });
    }

    #[cfg(feature = "share_nothing")]
    pub fn share_nothing(self, addr: &str) {
        let service = self.0;
        let n = num_cpus::get_physical();

        let handles: Vec<_> = (0..n)
            .map(|_| {
                let addr = addr.to_string();
                let value = service.clone();
                std::thread::spawn(move || {
                    let service = value.clone();

                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let local = tokio::task::LocalSet::new();

                    rt.block_on(local.run_until(async {
                        let socket = create_reuse_port_listener(&addr).unwrap();
                        let listener = socket.listen(get_somaxconn().unwrap()).unwrap();

                        loop {
                            let (stream, _) = listener.accept().await.unwrap();
                            let service = service.clone();
                            local.spawn_local(async move {
                                if let Err(_) = each_connection_loop(stream, service).await {
                                    //eprintln!("service err = {e:?}");
                                }
                            });
                        }
                    }))
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[cfg(feature = "io_uring_registry")]
    pub fn io_uring_registry(self, addr: &str) {
        let n = num_cpus::get_physical();
        let service = self.0;
        let handles: Vec<_> = (0..n)
            .map(|_| {
                let addr = addr.to_string();
                let value = service.clone();

                let service = service.clone();

                std::thread::spawn(move || {
                    let registry = FixedBufRegistry::new(
                        iter::repeat_with(|| Vec::with_capacity(BUF_SIZE)).take(POOL_SIZE),
                    );

                    let response_pool = std::rc::Rc::new(std::cell::RefCell::new(
                        (0..1000)
                            .map(|_| String::with_capacity(POOL_SIZE))
                            .collect::<Vec<_>>(),
                    ));
                    let service = service.clone();
                    tokio_uring::start(async move {
                        registry.register().expect("register failed");

                        let socket = create_reuse_port_listener(&addr).unwrap();
                        let listener = socket.listen(get_somaxconn().unwrap()).unwrap();
                        let listener = tokio_uring::net::TcpListener::from_std(listener.into_std().unwrap());

                        loop {
                            let (stream, _) = listener.accept().await.unwrap();
                            let registry = registry.clone();
                            let pool2 = response_pool.clone();
                            let service = service.clone();
                            tokio_uring::spawn(async move {
                                let mut index = 0;
                                let mut buf = None;
                                while index < POOL_SIZE {
                                    if let Some(b) = registry.check_out(index) {
                                        buf = Some(b);
                                        break;
                                    }
                                    index += 1;
                                }

                                let mut buf = match buf {
                                    Some(b) => b,
                                    None => return,
                                };

                                let mut response_buf = pool2
                                    .borrow_mut()
                                    .pop()
                                    .unwrap_or_else(|| String::with_capacity(1024));

                                let mut read_pos = 0;
                                let mut write_pos = 0;

                                loop {
                                    let (res, buf_back) = stream.read(buf).await;
                                    buf = buf_back;

                                    let n = match res {
                                        Ok(0) => return,
                                        Ok(n) => n,
                                        Err(_) => return,
                                    };

                                    let slice = &buf[..n];
                                    write_pos = n;

                                    let mut headers = [EMPTY_HEADER; 32];
                                    let mut req = Request::new(&mut headers);
                                    match req.parse(&slice[..write_pos]) {
                                        Ok(Status::Complete(_used)) => {
                                            let mut should_close = false;
                                            for header in req.headers.iter() {
                                                if header.name.eq_ignore_ascii_case("connection") {
                                                    if let Ok(val) = str::from_utf8(header.value) {
                                                        if val
                                                            .to_ascii_lowercase()
                                                            .contains("close")
                                                        {
                                                            should_close = true;
                                                        }
                                                    }
                                                }
                                            }

                                            let response = service.router(req);
                                            let connection_header = if should_close {
                                                "Connection: close".to_string()
                                            } else {
                                                "Connection: keep-alive".to_string()
                                            };
                                            response_buf.clear();

                                            let _ = write!(
                                                response_buf,
                                                "HTTP/1.1 {} OK\r\nContent-Length: {}\r\nContent-Type: {}\r\n",
                                                response.status_code,
                                                response.body.len(),
                                                response.content_type,
                                            );

                                            for (k, v) in response.headers {
                                                let _ = write!(response_buf, "{}: {}\r\n", k, v);
                                            }

                                            let _ = write!(
                                                response_buf,
                                                "{}\r\n\r\n{}",
                                                connection_header, response.body
                                            );
                                            let (res, _) = stream
                                                .write_all(response_buf.as_bytes().to_vec())
                                                .await;
                                            if res.is_err() || should_close {
                                                return;
                                            }
                                        }
                                        _ => return,
                                    }
                                }

                                pool2.borrow_mut().push(response_buf);
                            });
                        }
                    });
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }

    #[cfg(feature = "io_uring_pool")]
    pub fn io_uring_pool(self, addr: &str) {
        let n = num_cpus::get_physical();
        let service = self.0;

        let handles: Vec<_> = (0..n)
            .map(|_| {
                let addr = addr.to_string();

                let service = service.clone();

                let value = service.clone();
                std::thread::spawn(move || {
                    let pool = FixedBufPool::new(
                        iter::repeat_with(|| Vec::with_capacity(BUF_SIZE)).take(POOL_SIZE),
                    );
                    let response_pool = std::rc::Rc::new(std::cell::RefCell::new(
                        (0..1000)
                            .map(|_| String::with_capacity(POOL_SIZE))
                            .collect::<Vec<_>>(),
                    ));

                    let service = service.clone();

                    tokio_uring::start(async move {
                        pool.register().expect("register failed");

                        let socket = create_reuse_port_listener(&addr).unwrap();
                        let listener = socket.listen(get_somaxconn().unwrap()).unwrap();
                        let listener = tokio_uring::net::TcpListener::from_std(listener.into_std().unwrap());

                        loop {
                            let (stream, _) = listener.accept().await.unwrap();
                            let pool = pool.clone();
                            let pool2 = response_pool.clone();
                            let service = service.clone();

                            tokio_uring::spawn(async move {
                                let mut buf = match pool.try_next(BUF_SIZE) {
                                    Some(b) => b,
                                    None => return,
                                };

                                let mut response_buf = pool2
                                    .borrow_mut()
                                    .pop()
                                    .unwrap_or_else(|| String::with_capacity(POOL_SIZE));
                                let mut read_pos = 0;
                                let mut write_pos = 0;

                                loop {
                                    let (res, buf_back) = stream.read(buf).await;
                                    buf = buf_back;

                                    let n = match res {
                                        Ok(0) => return,
                                        Ok(n) => n,
                                        Err(_) => return,
                                    };

                                    let slice = &buf[..n];
                                    write_pos = n;

                                    let mut headers = [EMPTY_HEADER; 32];
                                    let mut req = Request::new(&mut headers);
                                    match req.parse(&slice[..write_pos]) {
                                        Ok(Status::Complete(_used)) => {
                                            let mut should_close = false;
                                            for header in req.headers.iter() {
                                                if header.name.eq_ignore_ascii_case("connection") {
                                                    if let Ok(val) = str::from_utf8(header.value) {
                                                        if val
                                                            .to_ascii_lowercase()
                                                            .contains("close")
                                                        {
                                                            should_close = true;
                                                        }
                                                    }
                                                }
                                            }

                                            let response = service.router(req);
                                            let connection_header = if should_close {
                                                "Connection: close".to_string()
                                            } else {
                                                "Connection: keep-alive".to_string()
                                            };

                                            response_buf.clear();
                                            let _ = write!(
                                                response_buf,
                                                "HTTP/1.1 {} OK\r\nContent-Length: {}\r\nContent-Type: {}\r\n",
                                                response.status_code,
                                                response.body.len(),
                                                response.content_type,
                                            );

                                            for (k, v) in response.headers {
                                                let _ = write!(response_buf, "{}: {}\r\n", k, v);
                                            }

                                            let _ = write!(
                                                response_buf,
                                                "{}\r\n\r\n{}",
                                                connection_header, response.body
                                            );
                                            let (res, _) = stream
                                                .write_all(response_buf.as_bytes().to_vec())
                                                .await;
                                            if res.is_err() || should_close {
                                                return;
                                            }
                                        }
                                        _ => return,
                                    }
                                }

                                pool2.borrow_mut().push(response_buf);
                            });
                        }
                    });
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }
    }
}

#[cfg(feature = "work_stealing")]
pub trait MaybeSend: Send {}
#[cfg(feature = "work_stealing")]
impl<T: Send> MaybeSend for T {}

#[cfg(feature = "share_nothing")]
pub trait MaybeSend {}
#[cfg(feature = "share_nothing")]
impl<T> MaybeSend for T {}

#[cfg(any(feature = "work_stealing", feature = "share_nothing"))]
async fn each_connection_loop<T: HttpService + MaybeSend>(
    mut stream: TcpStream,
    mut service: T,
) -> io::Result<()> {
    let mut req_buf = BytesMut::with_capacity(BUF_LEN);
    let mut rsp_buf = BytesMut::with_capacity(BUF_LEN);
    let mut body_buf = BytesMut::with_capacity(4096);

    loop {
        let n = stream.read_buf(&mut req_buf).await?;
        if n == 0 {
            break;
        }

        loop {
            let mut headers = [MaybeUninit::uninit(); request::MAX_HEADERS];
            let req = match request::decode(&mut headers, &mut req_buf, &mut stream)? {
                Some(req) => req,
                None => break,
            };

            reserve_buf(&mut rsp_buf);
            let mut rsp = Response::new(&mut body_buf);

            match service.router(req, &mut rsp).await {
                Ok(()) => response::encode(rsp, &mut rsp_buf),
                Err(e) => response::encode_error(e, &mut rsp_buf),
            }
        }

        stream.write_all_buf(&mut rsp_buf).await?;
    }

    stream.shutdown().await?;
    Ok(())
}

#[cfg(not(feature = "work_stealing"))]
fn create_reuse_port_listener(addr: &str) -> io::Result<TcpSocket> {
    let socket = TcpSocket::new_v4()?;
    socket.set_reuseaddr(true)?;
    socket.set_reuseport(true)?;
    socket.bind(addr.parse().unwrap())?;
    Ok(socket)
}

pub fn get_somaxconn() -> io::Result<u32> {
    let output = Command::new("sysctl").arg("net.core.somaxconn").output()?;

    if !output.status.success() {
        //return Err(io::Error::new(io::ErrorKind::Other, "sysctl failed"));
        return Ok(4096);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for part in stdout.split_whitespace() {
        if let Ok(val) = part.parse::<u32>() {
            return Ok(val);
        }
    }

    /* Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "could not parse somaxconn",
    )) */

    Ok(4096)
}

const BUF_LEN: usize = 4096 * 8;

#[inline]
pub(crate) fn reserve_buf(buf: &mut BytesMut) {
    let rem = buf.capacity() - buf.len();
    if rem < 1024 {
        buf.reserve(BUF_LEN - rem);
    }
}
